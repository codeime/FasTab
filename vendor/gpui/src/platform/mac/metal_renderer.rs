// Modified by Fastab: retire idle instance buffers without recaching late GPU
// completions, allocate path textures only when needed, and lazily create video
// texture caches with a borrowed Metal device.
// See FASTAB_PATCHES.md at the root of this vendored crate.

use super::metal_atlas::MetalAtlas;
use crate::{
    AtlasTextureId, Background, Bounds, ContentMask, DevicePixels, MonochromeSprite, PaintSurface,
    Path, Point, PolychromeSprite, PrimitiveBatch, Quad, ScaledPixels, Scene, Shadow, Size,
    Surface, Underline, point, size,
};
use anyhow::Result;
use block::ConcreteBlock;
use cocoa::{
    base::{NO, YES},
    foundation::{NSSize, NSUInteger},
    quartzcore::AutoresizingMask,
};

use core_foundation::{
    base::{CFAllocatorRef, TCFType, kCFAllocatorDefault},
    dictionary::CFDictionaryRef,
};
use core_video::{
    metal_texture::CVMetalTextureGetTexture,
    metal_texture_cache::{CVMetalTextureCache, CVMetalTextureCacheRef},
    pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    r#return::{CVReturn, kCVReturnInvalidArgument, kCVReturnSuccess},
};
use foreign_types::{ForeignType, ForeignTypeRef};
use metal::{
    CAMetalLayer, CommandQueue, MTLPixelFormat, MTLResourceOptions, NSRange,
    RenderPassColorAttachmentDescriptorRef,
};
use objc::{self, msg_send, sel, sel_impl};
use parking_lot::Mutex;

use std::{cell::Cell, ffi::c_void, mem, ptr, sync::Arc};

// Exported to metal
pub(crate) type PointF = crate::Point<f32>;

#[cfg(not(feature = "runtime_shaders"))]
const SHADERS_METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders.metallib"));
#[cfg(feature = "runtime_shaders")]
const SHADERS_SOURCE_FILE: &str = include_str!(concat!(env!("OUT_DIR"), "/stitched_shaders.metal"));
// Use 4x MSAA, all devices support it.
// https://developer.apple.com/documentation/metal/mtldevice/1433355-supportstexturesamplecount
const PATH_SAMPLE_COUNT: u32 = 4;

unsafe extern "C" {
    // core-video 0.4.3 declares this borrowed argument as an owned Device.
    // Moving a cloned Device into that C call abandons its Rust release.
    #[link_name = "CVMetalTextureCacheCreate"]
    fn create_metal_texture_cache_borrowed(
        allocator: CFAllocatorRef,
        cache_attributes: CFDictionaryRef,
        device: *mut metal::MTLDevice,
        texture_attributes: CFDictionaryRef,
        cache_out: *mut CVMetalTextureCacheRef,
    ) -> CVReturn;
}

fn create_surface_texture_cache(
    device: &metal::DeviceRef,
) -> Result<CVMetalTextureCache, CVReturn> {
    // SAFETY: this is the SDK's non-consuming device argument and Create +1 output.
    unsafe {
        create_surface_texture_cache_with(device, |device, cache_out| {
            create_metal_texture_cache_borrowed(
                kCFAllocatorDefault,
                ptr::null(),
                device,
                ptr::null(),
                cache_out,
            )
        })
    }
}

/// # Safety
/// `create` must borrow the device and, on success, transfer either one valid
/// CVMetalTextureCache reference or null through its output pointer. On failure
/// it must not transfer an owned reference.
unsafe fn create_surface_texture_cache_with(
    device: &metal::DeviceRef,
    create: impl FnOnce(*mut metal::MTLDevice, *mut CVMetalTextureCacheRef) -> CVReturn,
) -> Result<CVMetalTextureCache, CVReturn> {
    let mut cache = ptr::null_mut();
    let status = create(device.as_ptr(), &mut cache);
    if status != kCVReturnSuccess {
        return Err(status);
    }
    if cache.is_null() {
        return Err(kCVReturnInvalidArgument);
    }
    // The SDK borrows the device and transfers exactly the cacheOut +1.
    Ok(unsafe { CVMetalTextureCache::wrap_under_create_rule(cache) })
}

#[derive(Default)]
struct SurfaceTextureCache {
    cache: Option<CVMetalTextureCache>,
}

impl SurfaceTextureCache {
    fn for_batch(
        &mut self,
        device: &metal::DeviceRef,
        surface_count: usize,
    ) -> Result<Option<&CVMetalTextureCache>, CVReturn> {
        if surface_count == 0 {
            return Ok(None);
        }
        if self.cache.is_none() {
            self.cache = Some(create_surface_texture_cache(device)?);
        }
        Ok(self.cache.as_ref())
    }
}

pub type Context = Arc<Mutex<InstanceBufferPool>>;
pub type Renderer = MetalRenderer;

pub unsafe fn new_renderer(
    context: self::Context,
    _native_window: *mut c_void,
    _native_view: *mut c_void,
    _bounds: crate::Size<f32>,
    _transparent: bool,
) -> Renderer {
    MetalRenderer::new(context)
}

pub(crate) struct InstanceBufferPool {
    buffer_size: usize,
    buffers: Vec<metal::Buffer>,
    generation: u64,
}

impl Default for InstanceBufferPool {
    fn default() -> Self {
        Self {
            buffer_size: 2 * 1024 * 1024,
            buffers: Vec::new(),
            generation: 0,
        }
    }
}

pub(crate) struct InstanceBuffer {
    metal_buffer: metal::Buffer,
    size: usize,
    generation: u64,
}

impl InstanceBufferPool {
    /// Invalidate outstanding buffers and return idle buffers to drop after unlocking.
    fn trim(&mut self) -> Vec<metal::Buffer> {
        self.generation = self.generation.wrapping_add(1);
        mem::take(&mut self.buffers)
    }

    pub(crate) fn reset(&mut self, buffer_size: usize) -> Vec<metal::Buffer> {
        self.buffer_size = buffer_size;
        self.trim()
    }

    pub(crate) fn acquire(&mut self, device: &metal::Device) -> InstanceBuffer {
        let buffer = self.buffers.pop().unwrap_or_else(|| {
            device.new_buffer(
                self.buffer_size as u64,
                MTLResourceOptions::StorageModeManaged,
            )
        });
        InstanceBuffer {
            metal_buffer: buffer,
            size: self.buffer_size,
            generation: self.generation,
        }
    }

    /// Return a stale buffer to drop after unlocking, once its GPU work has completed.
    pub(crate) fn release(&mut self, buffer: InstanceBuffer) -> Option<metal::Buffer> {
        if buffer.size == self.buffer_size && buffer.generation == self.generation {
            self.buffers.push(buffer.metal_buffer);
            None
        } else {
            Some(buffer.metal_buffer)
        }
    }
}

pub(crate) struct MetalRenderer {
    device: metal::Device,
    layer: metal::MetalLayer,
    presents_with_transaction: bool,
    command_queue: CommandQueue,
    paths_rasterization_pipeline_state: metal::RenderPipelineState,
    path_sprites_pipeline_state: metal::RenderPipelineState,
    shadows_pipeline_state: metal::RenderPipelineState,
    quads_pipeline_state: metal::RenderPipelineState,
    underlines_pipeline_state: metal::RenderPipelineState,
    monochrome_sprites_pipeline_state: metal::RenderPipelineState,
    polychrome_sprites_pipeline_state: metal::RenderPipelineState,
    surfaces_pipeline_state: metal::RenderPipelineState,
    unit_vertices: metal::Buffer,
    #[allow(clippy::arc_with_non_send_sync)]
    instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    sprite_atlas: Arc<MetalAtlas>,
    core_video_texture_cache: SurfaceTextureCache,
    path_intermediate_texture: Option<metal::Texture>,
    path_intermediate_msaa_texture: Option<metal::Texture>,
    path_sample_count: u32,
}

impl Drop for MetalRenderer {
    fn drop(&mut self) {
        // The pool belongs to the application, so destroying a window otherwise
        // leaves its buffers resident. In-flight buffers remain owned by their
        // completed handlers, and their old generation prevents late recaching.
        let retired_buffers = {
            let mut pool = self.instance_buffer_pool.lock();
            pool.trim()
        };
        drop(retired_buffers);
    }
}

#[repr(C)]
pub struct PathRasterizationVertex {
    pub xy_position: Point<ScaledPixels>,
    pub st_position: Point<f32>,
    pub color: Background,
    pub bounds: Bounds<ScaledPixels>,
}

impl MetalRenderer {
    pub fn new(instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>) -> Self {
        // Prefer low‐power integrated GPUs on Intel Mac. On Apple
        // Silicon, there is only ever one GPU, so this is equivalent to
        // `metal::Device::system_default()`.
        let mut devices = metal::Device::all();
        devices.sort_by_key(|device| (device.is_removable(), device.is_low_power()));
        let Some(device) = devices.pop() else {
            log::error!("unable to access a compatible graphics device");
            std::process::exit(1);
        };

        let layer = metal::MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_opaque(false);
        layer.set_maximum_drawable_count(3);
        unsafe {
            let _: () = msg_send![&*layer, setAllowsNextDrawableTimeout: NO];
            let _: () = msg_send![&*layer, setNeedsDisplayOnBoundsChange: YES];
            let _: () = msg_send![
                &*layer,
                setAutoresizingMask: AutoresizingMask::WIDTH_SIZABLE
                    | AutoresizingMask::HEIGHT_SIZABLE
            ];
        }
        #[cfg(feature = "runtime_shaders")]
        let library = device
            .new_library_with_source(&SHADERS_SOURCE_FILE, &metal::CompileOptions::new())
            .expect("error building metal library");
        #[cfg(not(feature = "runtime_shaders"))]
        let library = device
            .new_library_with_data(SHADERS_METALLIB)
            .expect("error building metal library");

        fn to_float2_bits(point: PointF) -> u64 {
            let mut output = point.y.to_bits() as u64;
            output <<= 32;
            output |= point.x.to_bits() as u64;
            output
        }

        let unit_vertices = [
            to_float2_bits(point(0., 0.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(1., 1.)),
        ];
        let unit_vertices = device.new_buffer_with_data(
            unit_vertices.as_ptr() as *const c_void,
            mem::size_of_val(&unit_vertices) as u64,
            MTLResourceOptions::StorageModeManaged,
        );

        let paths_rasterization_pipeline_state = build_path_rasterization_pipeline_state(
            &device,
            &library,
            "paths_rasterization",
            "path_rasterization_vertex",
            "path_rasterization_fragment",
            MTLPixelFormat::BGRA8Unorm,
            PATH_SAMPLE_COUNT,
        );
        let path_sprites_pipeline_state = build_path_sprite_pipeline_state(
            &device,
            &library,
            "path_sprites",
            "path_sprite_vertex",
            "path_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let shadows_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "shadows",
            "shadow_vertex",
            "shadow_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let quads_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "quads",
            "quad_vertex",
            "quad_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let underlines_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "underlines",
            "underline_vertex",
            "underline_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let monochrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "monochrome_sprites",
            "monochrome_sprite_vertex",
            "monochrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let polychrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "polychrome_sprites",
            "polychrome_sprite_vertex",
            "polychrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let surfaces_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "surfaces",
            "surface_vertex",
            "surface_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );

        let command_queue = device.new_command_queue();
        let sprite_atlas = Arc::new(MetalAtlas::new(device.clone()));

        Self {
            device,
            layer,
            presents_with_transaction: false,
            command_queue,
            paths_rasterization_pipeline_state,
            path_sprites_pipeline_state,
            shadows_pipeline_state,
            quads_pipeline_state,
            underlines_pipeline_state,
            monochrome_sprites_pipeline_state,
            polychrome_sprites_pipeline_state,
            surfaces_pipeline_state,
            unit_vertices,
            instance_buffer_pool,
            sprite_atlas,
            core_video_texture_cache: SurfaceTextureCache::default(),
            path_intermediate_texture: None,
            path_intermediate_msaa_texture: None,
            path_sample_count: PATH_SAMPLE_COUNT,
        }
    }

    pub fn layer(&self) -> &metal::MetalLayerRef {
        &self.layer
    }

    pub fn layer_ptr(&self) -> *mut CAMetalLayer {
        self.layer.as_ptr()
    }

    pub fn sprite_atlas(&self) -> &Arc<MetalAtlas> {
        &self.sprite_atlas
    }

    pub fn set_presents_with_transaction(&mut self, presents_with_transaction: bool) {
        self.presents_with_transaction = presents_with_transaction;
        self.layer
            .set_presents_with_transaction(presents_with_transaction);
    }

    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        let size = NSSize {
            width: size.width.0.max(0) as f64,
            height: size.height.0.max(0) as f64,
        };
        let previous = self.layer.drawable_size();
        if size.width == previous.width && size.height == previous.height {
            return;
        }
        // A resized window does not need path targets until it paints a path.
        // Already encoded commands retain their old textures until GPU completion.
        self.path_intermediate_texture = None;
        self.path_intermediate_msaa_texture = None;
        unsafe {
            let _: () = msg_send![
                self.layer(),
                setDrawableSize: size
            ];
        }
    }

    fn prepare_path_intermediate_textures(&mut self, scene: &Scene, size: Size<DevicePixels>) {
        // We are uncertain when this happens, but sometimes size can be 0 here. Most likely before
        // the layout pass on window creation. Zero-sized texture creation causes SIGABRT.
        // https://github.com/zed-industries/zed/issues/36229
        if size.width.0 <= 0 || size.height.0 <= 0 {
            self.path_intermediate_texture = None;
            self.path_intermediate_msaa_texture = None;
            return;
        }
        // Retain a same-size target across frames without paths to avoid churn
        // when an AI icon appears intermittently. Resize and window drop retire it.
        if scene.paths.is_empty()
            || self
                .path_intermediate_texture
                .as_ref()
                .is_some_and(|texture| {
                    texture.width() == size.width.0 as u64
                        && texture.height() == size.height.0 as u64
                })
        {
            return;
        }

        let texture_descriptor = metal::TextureDescriptor::new();
        texture_descriptor.set_width(size.width.0 as u64);
        texture_descriptor.set_height(size.height.0 as u64);
        texture_descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
        texture_descriptor
            .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
        self.path_intermediate_texture = Some(self.device.new_texture(&texture_descriptor));

        if self.path_sample_count > 1 {
            let mut msaa_descriptor = texture_descriptor;
            msaa_descriptor.set_texture_type(metal::MTLTextureType::D2Multisample);
            msaa_descriptor.set_storage_mode(metal::MTLStorageMode::Private);
            msaa_descriptor.set_sample_count(self.path_sample_count as _);
            self.path_intermediate_msaa_texture = Some(self.device.new_texture(&msaa_descriptor));
        } else {
            self.path_intermediate_msaa_texture = None;
        }
    }

    pub fn update_transparency(&self, _transparent: bool) {
        // todo(mac)?
    }

    pub fn destroy(&self) {
        // nothing to do
    }

    pub fn draw(&mut self, scene: &Scene) {
        let layer = self.layer.clone();
        let viewport_size = layer.drawable_size();
        let viewport_size: Size<DevicePixels> = size(
            (viewport_size.width.ceil() as i32).into(),
            (viewport_size.height.ceil() as i32).into(),
        );
        if viewport_size.width.0 <= 0 || viewport_size.height.0 <= 0 {
            return;
        }
        let drawable = if let Some(drawable) = layer.next_drawable() {
            drawable
        } else {
            log::error!(
                "failed to retrieve next drawable, drawable size: {:?}",
                viewport_size
            );
            return;
        };
        self.prepare_path_intermediate_textures(scene, viewport_size);

        loop {
            let mut instance_buffer = self.instance_buffer_pool.lock().acquire(&self.device);

            let command_buffer =
                self.draw_primitives(scene, &mut instance_buffer, drawable, viewport_size);

            match command_buffer {
                Ok(command_buffer) => {
                    let instance_buffer_pool = self.instance_buffer_pool.clone();
                    let instance_buffer = Cell::new(Some(instance_buffer));
                    let block = ConcreteBlock::new(move |_| {
                        if let Some(instance_buffer) = instance_buffer.take() {
                            let retired_buffer = {
                                let mut pool = instance_buffer_pool.lock();
                                pool.release(instance_buffer)
                            };
                            drop(retired_buffer);
                        }
                    });
                    let block = block.copy();
                    command_buffer.add_completed_handler(&block);

                    if self.presents_with_transaction {
                        command_buffer.commit();
                        command_buffer.wait_until_scheduled();
                        drawable.present();
                    } else {
                        command_buffer.present_drawable(drawable);
                        command_buffer.commit();
                    }
                    return;
                }
                Err(err) => {
                    log::error!(
                        "failed to render: {}. retrying with larger instance buffer size",
                        err
                    );
                    let mut instance_buffer_pool = self.instance_buffer_pool.lock();
                    let buffer_size = instance_buffer_pool.buffer_size;
                    if buffer_size >= 256 * 1024 * 1024 {
                        log::error!("instance buffer size grew too large: {}", buffer_size);
                        break;
                    }
                    let retired_buffers = instance_buffer_pool.reset(buffer_size * 2);
                    log::info!(
                        "increased instance buffer size to {}",
                        instance_buffer_pool.buffer_size
                    );
                    drop(instance_buffer_pool);
                    drop(retired_buffers);
                }
            }
        }
    }

    fn draw_primitives(
        &mut self,
        scene: &Scene,
        instance_buffer: &mut InstanceBuffer,
        drawable: &metal::MetalDrawableRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<metal::CommandBuffer> {
        let command_queue = self.command_queue.clone();
        let command_buffer = command_queue.new_command_buffer();
        let alpha = if self.layer.is_opaque() { 1. } else { 0. };
        let mut instance_offset = 0;

        let mut command_encoder = new_command_encoder(
            command_buffer,
            drawable,
            viewport_size,
            |color_attachment| {
                color_attachment.set_load_action(metal::MTLLoadAction::Clear);
                color_attachment.set_clear_color(metal::MTLClearColor::new(0., 0., 0., alpha));
            },
        );

        for batch in scene.batches() {
            let ok = match batch {
                PrimitiveBatch::Shadows(shadows) => self.draw_shadows(
                    shadows,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Quads(quads) => self.draw_quads(
                    quads,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Paths(paths) => {
                    command_encoder.end_encoding();

                    let did_draw = self.draw_paths_to_intermediate(
                        paths,
                        instance_buffer,
                        &mut instance_offset,
                        viewport_size,
                        command_buffer,
                    );

                    command_encoder = new_command_encoder(
                        command_buffer,
                        drawable,
                        viewport_size,
                        |color_attachment| {
                            color_attachment.set_load_action(metal::MTLLoadAction::Load);
                        },
                    );

                    if did_draw {
                        self.draw_paths_from_intermediate(
                            paths,
                            instance_buffer,
                            &mut instance_offset,
                            viewport_size,
                            command_encoder,
                        )
                    } else {
                        false
                    }
                }
                PrimitiveBatch::Underlines(underlines) => self.draw_underlines(
                    underlines,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_monochrome_sprites(
                    texture_id,
                    sprites,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_polychrome_sprites(
                    texture_id,
                    sprites,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Surfaces(surfaces) => self.draw_surfaces(
                    surfaces,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
            };
            if !ok {
                command_encoder.end_encoding();
                anyhow::bail!(
                    "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} poly, {} surfaces",
                    scene.paths.len(),
                    scene.shadows.len(),
                    scene.quads.len(),
                    scene.underlines.len(),
                    scene.monochrome_sprites.len(),
                    scene.polychrome_sprites.len(),
                    scene.surfaces.len(),
                );
            }
        }

        command_encoder.end_encoding();

        instance_buffer.metal_buffer.did_modify_range(NSRange {
            location: 0,
            length: instance_offset as NSUInteger,
        });
        Ok(command_buffer.to_owned())
    }

    fn draw_paths_to_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_buffer: &metal::CommandBufferRef,
    ) -> bool {
        if paths.is_empty() {
            return true;
        }
        let Some(intermediate_texture) = &self.path_intermediate_texture else {
            return false;
        };

        let render_pass_descriptor = metal::RenderPassDescriptor::new();
        let color_attachment = render_pass_descriptor
            .color_attachments()
            .object_at(0)
            .unwrap();
        color_attachment.set_load_action(metal::MTLLoadAction::Clear);
        color_attachment.set_clear_color(metal::MTLClearColor::new(0., 0., 0., 0.));

        if let Some(msaa_texture) = &self.path_intermediate_msaa_texture {
            color_attachment.set_texture(Some(msaa_texture));
            color_attachment.set_resolve_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::MultisampleResolve);
        } else {
            color_attachment.set_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::Store);
        }

        let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
        command_encoder.set_render_pipeline_state(&self.paths_rasterization_pipeline_state);

        align_offset(instance_offset);
        let mut vertices = Vec::new();
        for path in paths {
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                xy_position: v.xy_position,
                st_position: v.st_position,
                color: path.color,
                bounds: path.bounds.intersect(&path.content_mask.bounds),
            }));
        }
        let vertices_bytes_len = mem::size_of_val(vertices.as_slice());
        let next_offset = *instance_offset + vertices_bytes_len;
        if next_offset > instance_buffer.size {
            command_encoder.end_encoding();
            return false;
        }
        command_encoder.set_vertex_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            PathRasterizationInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };
        unsafe {
            ptr::copy_nonoverlapping(
                vertices.as_ptr() as *const u8,
                buffer_contents,
                vertices_bytes_len,
            );
        }
        command_encoder.draw_primitives(
            metal::MTLPrimitiveType::Triangle,
            0,
            vertices.len() as u64,
        );
        *instance_offset = next_offset;

        command_encoder.end_encoding();
        true
    }

    fn draw_shadows(
        &self,
        shadows: &[Shadow],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if shadows.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.shadows_pipeline_state);
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            ShadowInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let shadow_bytes_len = mem::size_of_val(shadows);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + shadow_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                shadows.as_ptr() as *const u8,
                buffer_contents,
                shadow_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            shadows.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_quads(
        &self,
        quads: &[Quad],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if quads.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.quads_pipeline_state);
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            QuadInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let quad_bytes_len = mem::size_of_val(quads);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + quad_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(quads.as_ptr() as *const u8, buffer_contents, quad_bytes_len);
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            quads.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_paths_from_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        let Some(first_path) = paths.first() else {
            return true;
        };

        let Some(ref intermediate_texture) = self.path_intermediate_texture else {
            return false;
        };

        command_encoder.set_render_pipeline_state(&self.path_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.set_fragment_texture(
            SpriteInputIndex::AtlasTexture as u64,
            Some(intermediate_texture),
        );

        // When copying paths from the intermediate texture to the drawable,
        // each pixel must only be copied once, in case of transparent paths.
        //
        // If all paths have the same draw order, then their bounds are all
        // disjoint, so we can copy each path's bounds individually. If this
        // batch combines different draw orders, we perform a single copy
        // for a minimal spanning rect.
        let sprites;
        if paths.last().unwrap().order == first_path.order {
            sprites = paths
                .iter()
                .map(|path| PathSprite {
                    bounds: path.clipped_bounds(),
                })
                .collect();
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            sprites = vec![PathSprite { bounds }];
        }

        align_offset(instance_offset);
        let sprite_bytes_len = mem::size_of_val(sprites.as_slice());
        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };
        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;

        true
    }

    fn draw_underlines(
        &self,
        underlines: &[Underline],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if underlines.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.underlines_pipeline_state);
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            UnderlineInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let underline_bytes_len = mem::size_of_val(underlines);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + underline_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                underlines.as_ptr() as *const u8,
                buffer_contents,
                underline_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            underlines.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_monochrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: &[MonochromeSprite],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if sprites.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        let sprite_bytes_len = mem::size_of_val(sprites);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.monochrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_polychrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: &[PolychromeSprite],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if sprites.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.polychrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        let sprite_bytes_len = mem::size_of_val(sprites);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_surfaces(
        &mut self,
        surfaces: &[PaintSurface],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        let Some(texture_cache) = self
            .core_video_texture_cache
            .for_batch(&self.device, surfaces.len())
            .unwrap()
        else {
            return true;
        };
        command_encoder.set_render_pipeline_state(&self.surfaces_pipeline_state);
        command_encoder.set_vertex_buffer(
            SurfaceInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_bytes(
            SurfaceInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        for surface in surfaces {
            let texture_size = size(
                DevicePixels::from(surface.image_buffer.get_width() as i32),
                DevicePixels::from(surface.image_buffer.get_height() as i32),
            );

            assert_eq!(
                surface.image_buffer.get_pixel_format(),
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            );

            let y_texture = texture_cache
                .create_texture_from_image(
                    surface.image_buffer.as_concrete_TypeRef(),
                    None,
                    MTLPixelFormat::R8Unorm,
                    surface.image_buffer.get_width_of_plane(0),
                    surface.image_buffer.get_height_of_plane(0),
                    0,
                )
                .unwrap();
            let cb_cr_texture = texture_cache
                .create_texture_from_image(
                    surface.image_buffer.as_concrete_TypeRef(),
                    None,
                    MTLPixelFormat::RG8Unorm,
                    surface.image_buffer.get_width_of_plane(1),
                    surface.image_buffer.get_height_of_plane(1),
                    1,
                )
                .unwrap();

            align_offset(instance_offset);
            let next_offset = *instance_offset + mem::size_of::<Surface>();
            if next_offset > instance_buffer.size {
                return false;
            }

            command_encoder.set_vertex_buffer(
                SurfaceInputIndex::Surfaces as u64,
                Some(&instance_buffer.metal_buffer),
                *instance_offset as u64,
            );
            command_encoder.set_vertex_bytes(
                SurfaceInputIndex::TextureSize as u64,
                mem::size_of_val(&texture_size) as u64,
                &texture_size as *const Size<DevicePixels> as *const _,
            );
            // let y_texture = y_texture.get_texture().unwrap().
            command_encoder.set_fragment_texture(SurfaceInputIndex::YTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(y_texture.as_concrete_TypeRef());
                Some(metal::TextureRef::from_ptr(texture as *mut _))
            });
            command_encoder.set_fragment_texture(SurfaceInputIndex::CbCrTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(cb_cr_texture.as_concrete_TypeRef());
                Some(metal::TextureRef::from_ptr(texture as *mut _))
            });

            unsafe {
                let buffer_contents = (instance_buffer.metal_buffer.contents() as *mut u8)
                    .add(*instance_offset)
                    as *mut SurfaceBounds;
                ptr::write(
                    buffer_contents,
                    SurfaceBounds {
                        bounds: surface.bounds,
                        content_mask: surface.content_mask.clone(),
                    },
                );
            }

            command_encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 6);
            *instance_offset = next_offset;
        }
        true
    }
}

fn new_command_encoder<'a>(
    command_buffer: &'a metal::CommandBufferRef,
    drawable: &'a metal::MetalDrawableRef,
    viewport_size: Size<DevicePixels>,
    configure_color_attachment: impl Fn(&RenderPassColorAttachmentDescriptorRef),
) -> &'a metal::RenderCommandEncoderRef {
    let render_pass_descriptor = metal::RenderPassDescriptor::new();
    let color_attachment = render_pass_descriptor
        .color_attachments()
        .object_at(0)
        .unwrap();
    color_attachment.set_texture(Some(drawable.texture()));
    color_attachment.set_store_action(metal::MTLStoreAction::Store);
    configure_color_attachment(color_attachment);

    let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
    command_encoder.set_viewport(metal::MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: i32::from(viewport_size.width) as f64,
        height: i32::from(viewport_size.height) as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    command_encoder
}

fn build_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::One);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

fn build_path_sprite_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::One);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

fn build_path_rasterization_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    path_sample_count: u32,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    if path_sample_count > 1 {
        descriptor.set_raster_sample_count(path_sample_count as _);
        descriptor.set_alpha_to_coverage_enabled(false);
    }
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

// Align to multiples of 256 make Metal happy.
fn align_offset(offset: &mut usize) {
    *offset = (*offset).div_ceil(256) * 256;
}

#[repr(C)]
enum ShadowInputIndex {
    Vertices = 0,
    Shadows = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum QuadInputIndex {
    Vertices = 0,
    Quads = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum UnderlineInputIndex {
    Vertices = 0,
    Underlines = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum SpriteInputIndex {
    Vertices = 0,
    Sprites = 1,
    ViewportSize = 2,
    AtlasTextureSize = 3,
    AtlasTexture = 4,
}

#[repr(C)]
enum SurfaceInputIndex {
    Vertices = 0,
    Surfaces = 1,
    ViewportSize = 2,
    TextureSize = 3,
    YTexture = 4,
    CbCrTexture = 5,
}

#[repr(C)]
enum PathRasterizationInputIndex {
    Vertices = 0,
    ViewportSize = 1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PathSprite {
    pub bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct SurfaceBounds {
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
}

#[cfg(feature = "test-support")]
#[allow(missing_docs)]
pub(crate) mod fastab_resource_fixtures {
    use super::*;

    #[cfg(feature = "runtime_shaders")]
    pub fn generated_shader_bindings_exclude_host_ffi() {
        // Inspect the build output embedded in this GPUI dependency, including
        // the exact stitched source MetalRenderer passes to the Metal compiler.
        let header = include_str!(concat!(env!("OUT_DIR"), "/scene.h"));
        assert!(header.contains("#define SCENE_H"));
        assert!(header.contains("SurfaceBounds"));
        assert!(
            !header
                .split_ascii_whitespace()
                .any(|token| token == "extern"),
            "the generated shader header must not declare host externs"
        );
        assert!(SHADERS_SOURCE_FILE.starts_with(header));
        assert!(SHADERS_SOURCE_FILE.contains("vertex SurfaceVertexOutput surface_vertex("));
        for host_symbol in [
            "create_metal_texture_cache_borrowed",
            "CVMetalTextureCacheCreate",
            "CVReturn",
            "CFAllocatorRef",
            "CFDictionaryRef",
            "CVMetalTextureCacheRef",
            "MTLDevice",
        ] {
            assert!(
                !header.contains(host_symbol),
                "host FFI symbol {host_symbol} leaked into generated scene.h"
            );
            assert!(
                !SHADERS_SOURCE_FILE.contains(host_symbol),
                "host FFI symbol {host_symbol} leaked into the runtime Metal source"
            );
        }
    }

    static CACHE_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

    fn device_retain_count(device: &metal::DeviceRef) -> usize {
        unsafe { msg_send![device, retainCount] }
    }

    // Hardware fixtures: collect in the workspace harness as explicitly ignored
    // tests, then run on a Metal-capable host. Never silently skip missing Metal.
    pub fn surface_cache_creation_borrows_device_on_success_and_error() {
        let _guard = CACHE_FIXTURE_LOCK.lock();
        objc::rc::autoreleasepool(|| {
            let device = metal::Device::system_default().expect("a Metal device is required");
            let before = device_retain_count(&device);
            // Inspect at the actual FFI boundary, before CoreVideo can acquire
            // its own legitimate references or initialize driver caches.
            let cache = unsafe {
                create_surface_texture_cache_with(&device, |pointer, cache_out| {
                    assert_eq!(pointer, device.as_ptr());
                    assert_eq!(device_retain_count(&device), before);
                    create_metal_texture_cache_borrowed(
                        kCFAllocatorDefault,
                        ptr::null(),
                        pointer,
                        ptr::null(),
                        cache_out,
                    )
                })
            }
            .expect("the real CoreVideo cache must be created");
            drop(cache);

            let before_error = device_retain_count(&device);
            let failure = core_video::r#return::kCVReturnError;
            let result = unsafe {
                create_surface_texture_cache_with(&device, |pointer, cache_out| {
                    assert_eq!(pointer, device.as_ptr());
                    assert_eq!(device_retain_count(&device), before_error);
                    assert!((*cache_out).is_null());
                    failure
                })
            };
            assert_eq!(result.unwrap_err(), failure);
            assert_eq!(device_retain_count(&device), before_error);

            let result =
                unsafe { create_surface_texture_cache_with(&device, |_, _| kCVReturnSuccess) };
            assert_eq!(result.unwrap_err(), kCVReturnInvalidArgument);
            assert_eq!(device_retain_count(&device), before_error);
        });
    }

    pub fn surface_cache_initializes_lazily_reuses_and_releases_its_owner() {
        let _guard = CACHE_FIXTURE_LOCK.lock();
        objc::rc::autoreleasepool(|| {
            let device = metal::Device::system_default().expect("a Metal device is required");
            let before = device_retain_count(&device);
            let mut slot = SurfaceTextureCache::default();
            assert!(slot.cache.is_none());
            for _ in 0..8 {
                assert!(slot.for_batch(&device, 0).unwrap().is_none());
                assert!(slot.cache.is_none());
            }
            assert_eq!(device_retain_count(&device), before);

            let cache = slot.for_batch(&device, 1).unwrap().unwrap();
            let observer = cache.clone();
            let pointer = cache.as_concrete_TypeRef();
            let owned_count = observer.retain_count();
            for _ in 0..8 {
                assert!(slot.for_batch(&device, 0).unwrap().is_none());
                assert_eq!(
                    slot.for_batch(&device, 2)
                        .unwrap()
                        .unwrap()
                        .as_concrete_TypeRef(),
                    pointer
                );
            }
            assert_eq!(observer.retain_count(), owned_count);
            drop(slot);
            assert_eq!(
                observer.retain_count(),
                owned_count - 1,
                "retiring the lazy cache must release its Create reference"
            );
            drop(observer);

            // CoreVideo may hold transient device references of its own, so
            // compare against a settled count. A consumed device argument
            // leaks one reference per cache, i.e. at least one per cycle.
            const CYCLES: usize = 32;
            let settled = device_retain_count(&device);
            for _ in 0..CYCLES {
                objc::rc::autoreleasepool(|| {
                    let mut slot = SurfaceTextureCache::default();
                    assert!(slot.for_batch(&device, 1).unwrap().is_some());
                });
            }
            let after = device_retain_count(&device);
            assert!(
                after < settled + CYCLES / 2,
                "{CYCLES} lazy caches left the device at {after} references, from {settled}"
            );
        });
    }
}

#[cfg(test)]
mod instance_buffer_pool_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    fn path_scene() -> Scene {
        let mut path = Path::new(point(crate::px(0.), crate::px(0.)));
        path.line_to(point(crate::px(8.), crate::px(0.)));
        path.line_to(point(crate::px(0.), crate::px(8.)));
        path.content_mask.bounds = path.bounds;
        path.color = crate::rgb(0xffffff).into();
        let mut scene = Scene::default();
        scene.paths.push(path.scale(1.));
        scene
    }

    #[test]
    #[ignore = "requires a Metal-capable macOS host"]
    fn path_textures_are_lazy_reused_and_invalidated_by_resize() {
        objc::rc::autoreleasepool(|| {
            let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
            let mut renderer = MetalRenderer::new(pool);
            let empty = Scene::default();
            let paths = path_scene();
            let original_size = size(DevicePixels(16), DevicePixels(16));
            let resized = size(DevicePixels(32), DevicePixels(24));

            renderer.update_drawable_size(original_size);
            renderer.prepare_path_intermediate_textures(&empty, original_size);
            assert!(renderer.path_intermediate_texture.is_none());
            assert!(renderer.path_intermediate_msaa_texture.is_none());

            renderer.prepare_path_intermediate_textures(&paths, original_size);
            let texture = renderer.path_intermediate_texture.as_ref().unwrap().clone();
            let msaa = renderer
                .path_intermediate_msaa_texture
                .as_ref()
                .unwrap()
                .clone();
            assert_eq!((texture.width(), texture.height()), (16, 16));
            assert_eq!(msaa.sample_count(), PATH_SAMPLE_COUNT as u64);
            renderer.update_drawable_size(original_size);
            renderer.prepare_path_intermediate_textures(&empty, original_size);
            renderer.prepare_path_intermediate_textures(&paths, original_size);
            assert_eq!(
                renderer
                    .path_intermediate_texture
                    .as_ref()
                    .unwrap()
                    .as_ptr(),
                texture.as_ptr()
            );
            assert_eq!(
                renderer
                    .path_intermediate_msaa_texture
                    .as_ref()
                    .unwrap()
                    .as_ptr(),
                msaa.as_ptr()
            );

            renderer.update_drawable_size(resized);
            renderer.prepare_path_intermediate_textures(&empty, resized);
            assert!(renderer.path_intermediate_texture.is_none());
            assert!(renderer.path_intermediate_msaa_texture.is_none());
            renderer.prepare_path_intermediate_textures(&paths, resized);
            let replacement = renderer.path_intermediate_texture.as_ref().unwrap();
            assert_eq!((replacement.width(), replacement.height()), (32, 24));
            assert_ne!(replacement.as_ptr(), texture.as_ptr());

            for invalid in [
                size(DevicePixels(0), DevicePixels(24)),
                size(DevicePixels(32), DevicePixels(-1)),
            ] {
                renderer.update_drawable_size(invalid);
                renderer.prepare_path_intermediate_textures(&paths, invalid);
                assert!(renderer.path_intermediate_texture.is_none());
                assert!(renderer.path_intermediate_msaa_texture.is_none());
                renderer.update_drawable_size(original_size);
                renderer.prepare_path_intermediate_textures(&paths, original_size);
                assert_eq!(
                    renderer.path_intermediate_texture.as_ref().unwrap().width(),
                    16
                );
            }
        });
    }

    #[test]
    #[ignore = "requires a Metal-capable macOS host"]
    fn committed_path_rasterization_survives_resize_and_resolves_msaa() {
        struct ReleaseGate {
            event: metal::SharedEvent,
            deadline: mpsc::Sender<()>,
        }
        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                self.event.set_signaled_value(1);
                let _ = self.deadline.send(());
            }
        }

        objc::rc::autoreleasepool(|| {
            let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
            let mut renderer = MetalRenderer::new(pool.clone());
            let viewport = size(DevicePixels(16), DevicePixels(16));
            let scene = path_scene();
            renderer.update_drawable_size(viewport);
            renderer.prepare_path_intermediate_textures(&scene, viewport);
            let event = renderer.device.new_shared_event();
            let command = renderer.command_queue.new_command_buffer().to_owned();
            command.encode_wait_for_event(&event, 1);
            let mut vertices = pool.lock().acquire(&renderer.device);
            let mut offset = 0;
            let output = renderer
                .device
                .new_buffer(256, MTLResourceOptions::StorageModeShared);
            // Drain the render-pass descriptor's autoreleased references before
            // resizing so it cannot accidentally keep the old textures alive.
            objc::rc::autoreleasepool(|| {
                assert!(renderer.draw_paths_to_intermediate(
                    &scene.paths,
                    &mut vertices,
                    &mut offset,
                    viewport,
                    &command,
                ));
                vertices
                    .metal_buffer
                    .did_modify_range(NSRange::new(0, offset as u64));
                let blit = command.new_blit_command_encoder();
                blit.copy_from_texture_to_buffer(
                    renderer.path_intermediate_texture.as_ref().unwrap(),
                    0,
                    0,
                    metal::MTLOrigin { x: 1, y: 1, z: 0 },
                    metal::MTLSize::new(1, 1, 1),
                    &output,
                    0,
                    256,
                    256,
                    metal::MTLBlitOption::None,
                );
                blit.end_encoding();
            });
            let (completed_tx, completed_rx) = mpsc::channel();
            let completion = ConcreteBlock::new(move |_| {
                let _ = completed_tx.send(());
            })
            .copy();
            command.add_completed_handler(&completion);
            let deadline_event = event.to_owned();
            let (deadline_tx, deadline_rx) = mpsc::channel();
            let deadline = std::thread::spawn(move || {
                let timed_out = matches!(
                    deadline_rx.recv_timeout(Duration::from_millis(500)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                );
                deadline_event.set_signaled_value(1);
                timed_out
            });
            let gate = ReleaseGate {
                event,
                deadline: deadline_tx,
            };
            command.commit();

            renderer.update_drawable_size(size(DevicePixels(32), DevicePixels(24)));
            assert!(renderer.path_intermediate_texture.is_none());
            assert!(renderer.path_intermediate_msaa_texture.is_none());
            assert_eq!(gate.event.signaled_value(), 0);
            assert_eq!(completed_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
            drop(gate);
            assert!(
                !deadline.join().unwrap(),
                "Resize exceeded the GPU gate deadline"
            );
            completed_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("Path rasterization did not complete");
            assert_eq!(command.status(), metal::MTLCommandBufferStatus::Completed);
            let pixel = unsafe { std::slice::from_raw_parts(output.contents().cast::<u8>(), 4) };
            assert_eq!(pixel, &[255; 4]);
        });
    }

    // Encode real GPU work, but let the caller commit after closing a renderer.
    // The completion owns the buffer until Metal is done, as in the draw path.
    fn encode_buffer_completion(
        renderer: &MetalRenderer,
        pool: Arc<Mutex<InstanceBufferPool>>,
        buffer: InstanceBuffer,
    ) -> (metal::CommandBuffer, mpsc::Receiver<(bool, u8)>) {
        encode_buffer_completion_after_event(renderer, pool, buffer, None)
    }

    fn encode_buffer_completion_after_event(
        renderer: &MetalRenderer,
        pool: Arc<Mutex<InstanceBufferPool>>,
        buffer: InstanceBuffer,
        event: Option<&metal::SharedEventRef>,
    ) -> (metal::CommandBuffer, mpsc::Receiver<(bool, u8)>) {
        let command = renderer.command_queue.new_command_buffer().to_owned();
        if let Some(event) = event {
            command.encode_wait_for_event(event, 1);
        }
        let encoder = command.new_blit_command_encoder();
        encoder.fill_buffer(&buffer.metal_buffer, NSRange::new(0, 4), 0x5a);
        encoder.synchronize_resource(&buffer.metal_buffer);
        encoder.end_encoding();

        let (tx, rx) = mpsc::channel();
        let buffer = Cell::new(Some(buffer));
        let completion = ConcreteBlock::new(move |_| {
            if let Some(buffer) = buffer.take() {
                // The synchronize command has completed before this handler.
                let value = unsafe { *buffer.metal_buffer.contents().cast::<u8>() };
                let retired = {
                    let mut pool = pool.lock();
                    pool.release(buffer)
                };
                let cached = retired.is_none();
                drop(retired);
                let _ = tx.send((cached, value));
            }
        })
        .copy();
        command.add_completed_handler(&completion);
        (command, rx)
    }

    fn finish_buffer_completion(
        completion: (metal::CommandBuffer, mpsc::Receiver<(bool, u8)>),
        should_cache: bool,
    ) {
        let (command, rx) = completion;
        command.commit();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10))
                .expect("Metal buffer completion timed out"),
            (should_cache, 0x5a),
        );
    }

    #[test]
    #[ignore = "requires a Metal-capable macOS host"]
    fn late_gpu_completion_does_not_repopulate_closed_renderer_pool() {
        objc::rc::autoreleasepool(|| {
            let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
            let renderer = MetalRenderer::new(pool.clone());
            let pending = pool.lock().acquire(&renderer.device);
            let idle = pool.lock().acquire(&renderer.device);
            assert!(pool.lock().release(idle).is_none());
            let completion = encode_buffer_completion(&renderer, pool.clone(), pending);

            drop(renderer);
            assert!(pool.lock().buffers.is_empty());
            finish_buffer_completion(completion, false);
            assert!(pool.lock().buffers.is_empty());
        });
    }

    #[test]
    #[ignore = "requires a Metal-capable macOS host"]
    fn committed_gpu_work_finishes_without_recaching_after_renderer_closes() {
        // Always unblock the GPU, including assertions and timeout unwinds.
        struct ReleaseGate {
            event: metal::SharedEvent,
            release_deadline: mpsc::Sender<()>,
        }

        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                self.event.set_signaled_value(1);
                let _ = self.release_deadline.send(());
            }
        }

        objc::rc::autoreleasepool(|| {
            let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
            let renderer = MetalRenderer::new(pool.clone());
            let pending = pool.lock().acquire(&renderer.device);
            let idle = pool.lock().acquire(&renderer.device);
            assert!(pool.lock().release(idle).is_none());
            let event = renderer.device.new_shared_event();
            assert_eq!(event.signaled_value(), 0);
            let (command, completed_rx) = encode_buffer_completion_after_event(
                &renderer,
                pool.clone(),
                pending,
                Some(&event),
            );
            // A separate CPU deadline also opens the gate if renderer teardown
            // blocks. This must fire well before the GPU's own hang watchdog.
            let deadline_event = event.to_owned();
            let (release_deadline, deadline_rx) = mpsc::channel();
            let deadline = std::thread::spawn(move || {
                let timed_out = matches!(
                    deadline_rx.recv_timeout(Duration::from_millis(500)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                );
                deadline_event.set_signaled_value(1);
                timed_out
            });
            // Declared after the command so an unwind signals before releasing
            // the outstanding command and its queue.
            let gate = ReleaseGate {
                event,
                release_deadline,
            };

            command.commit();
            // Do not wait for a scheduled callback: some drivers delay it while
            // the shared-event wait is pending, until their GPU timeout fires.
            assert!(matches!(
                command.status(),
                metal::MTLCommandBufferStatus::Committed | metal::MTLCommandBufferStatus::Scheduled
            ));
            assert_eq!(completed_rx.try_recv(), Err(mpsc::TryRecvError::Empty));

            // The command is already committed, but its fill cannot run until
            // the CPU signals the shared event. Close with that work in flight.
            drop(renderer);
            assert!(pool.lock().buffers.is_empty());
            assert_eq!(gate.event.signaled_value(), 0);
            assert_eq!(completed_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
            drop(gate);
            assert!(
                !deadline
                    .join()
                    .expect("Metal gate deadline thread panicked"),
                "Renderer teardown exceeded the gate deadline"
            );

            assert_eq!(
                completed_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("Committed Metal work did not finish after opening the gate"),
                (false, 0x5a),
            );
            assert_eq!(command.status(), metal::MTLCommandBufferStatus::Completed);
            assert!(pool.lock().buffers.is_empty());
        });
    }

    #[test]
    #[ignore = "requires a Metal-capable macOS host"]
    fn reopening_preserves_grown_buffer_size_and_resumes_reuse() {
        objc::rc::autoreleasepool(|| {
            let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
            let renderer = MetalRenderer::new(pool.clone());
            let old_buffer = pool.lock().acquire(&renderer.device);
            let grown_size = old_buffer.size * 2;
            let retired = {
                let mut pool = pool.lock();
                pool.reset(grown_size)
            };
            drop(retired);
            finish_buffer_completion(
                encode_buffer_completion(&renderer, pool.clone(), old_buffer),
                false,
            );
            let grown_buffer = pool.lock().acquire(&renderer.device);
            assert_eq!(grown_buffer.metal_buffer.length(), grown_size as u64);
            assert!(pool.lock().release(grown_buffer).is_none());
            drop(renderer);
            assert!(pool.lock().buffers.is_empty());

            let reopened = MetalRenderer::new(pool.clone());
            let buffer = pool.lock().acquire(&reopened.device);
            assert_eq!(buffer.metal_buffer.length(), grown_size as u64);
            let buffer_address = buffer.metal_buffer.as_ptr();
            finish_buffer_completion(
                encode_buffer_completion(&reopened, pool.clone(), buffer),
                true,
            );
            let reused = pool.lock().acquire(&reopened.device);
            assert_eq!(reused.metal_buffer.as_ptr(), buffer_address);
        });
    }

    #[test]
    #[ignore = "requires a Metal-capable macOS host"]
    fn closing_one_renderer_keeps_another_renderers_buffer_usable() {
        objc::rc::autoreleasepool(|| {
            let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
            let closing = MetalRenderer::new(pool.clone());
            let surviving = MetalRenderer::new(pool.clone());
            let outstanding = pool.lock().acquire(&surviving.device);

            drop(closing);
            // The surviving renderer can still submit its previously acquired
            // buffer; trimming must not release or mutate that allocation.
            finish_buffer_completion(
                encode_buffer_completion(&surviving, pool.clone(), outstanding),
                false,
            );
            let next_frame = pool.lock().acquire(&surviving.device);
            finish_buffer_completion(
                encode_buffer_completion(&surviving, pool.clone(), next_frame),
                true,
            );
            assert_eq!(pool.lock().buffers.len(), 1);
        });
    }
}
