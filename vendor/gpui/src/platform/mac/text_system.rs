use crate::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun,
    FontStyle, FontWeight, GlyphId, LineLayout, Pixels, PlatformTextSystem, Point,
    RenderGlyphParams, Result, SUBPIXEL_VARIANTS_X, ShapedGlyph, ShapedRun, SharedString, Size,
    point, px, size, swap_rgba_pa_to_bgra,
};
use anyhow::anyhow;
use cocoa::appkit::CGFloat;
use collections::HashMap;
use core_foundation::{
    array::CFArray,
    attributed_string::CFMutableAttributedString,
    base::{CFRange, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use core_graphics::{
    base::{CGGlyph, kCGImageAlphaPremultipliedLast},
    color_space::CGColorSpace,
    context::{CGContext, CGTextDrawingMode},
    display::CGPoint,
};
use core_text::{
    font::CTFont,
    font_descriptor::{
        CTFontDescriptor, kCTFontSlantTrait, kCTFontSymbolicTrait, kCTFontWeightTrait,
        kCTFontWidthTrait,
    },
    line::CTLine,
    string_attributes::kCTFontAttributeName,
};
use font_kit::{
    font::Font as FontKitFont,
    handle::Handle,
    hinting::HintingOptions,
    metrics::Metrics,
    properties::{Properties, Style as FontkitStyle, Weight as FontkitWeight},
    source::SystemSource,
    sources::mem::MemSource,
};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    transform2d::Transform2F,
    vector::{Vector2F, Vector2I},
};
use smallvec::SmallVec;
use std::{borrow::Cow, char, convert::TryFrom, sync::Arc};

use super::open_type::apply_features_and_fallbacks;

#[allow(non_upper_case_globals)]
const kCGImageAlphaOnly: u32 = 7;

pub(crate) struct MacTextSystem(RwLock<MacTextSystemState>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontKey {
    font_family: SharedString,
    font_features: FontFeatures,
    font_fallbacks: Option<FontFallbacks>,
}

struct FontCandidate {
    properties: Option<Properties>,
    postscript_name: String,
    state: FontCandidateState,
    previous_source: Option<FontSource>,
}

enum FontCandidateState {
    // A matched, file-backed system descriptor. No CTFont has been requested
    // for this face yet, and its CSS properties are deliberately unknown.
    Deferred(CTFontDescriptor),
    Pending {
        descriptor: CTFontDescriptor,
        point_size: f64,
    },
    // An add_fonts face built from font data. Its descriptor carries no file
    // URL, so recreating from it can resolve a same-named system font instead.
    Resident(FontKitFont),
    Loaded(FontId),
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontSource {
    Candidate(usize),
    Loaded(FontId),
}

struct MacTextSystemState {
    memory_source: MemSource,
    system_source: SystemSource,
    fonts: Vec<FontKitFont>,
    font_candidates: Vec<FontCandidate>,
    font_selections: HashMap<Font, FontId>,
    font_sources_by_postscript_name: HashMap<String, FontSource>,
    font_candidates_by_font_key: HashMap<FontKey, SmallVec<[usize; 4]>>,
    postscript_names_by_font_id: HashMap<FontId, String>,
    /// UTF-16 indices of ZWNJS
    zwnjs_scratch_space: Vec<usize>,
}

impl MacTextSystem {
    pub(crate) fn new() -> Self {
        Self(RwLock::new(MacTextSystemState {
            memory_source: MemSource::empty(),
            system_source: SystemSource::new(),
            fonts: Vec::new(),
            font_candidates: Vec::new(),
            font_selections: HashMap::default(),
            font_sources_by_postscript_name: HashMap::default(),
            font_candidates_by_font_key: HashMap::default(),
            postscript_names_by_font_id: HashMap::default(),
            zwnjs_scratch_space: Vec::new(),
        }))
    }
}

impl Default for MacTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for MacTextSystem {
    fn release_idle_caches(&self) -> usize {
        // Only layout_line uses this scratch, under the same write lock. Its
        // returned glyphs own their indices; font identities remain untouched.
        let scratch = {
            let mut state = self.0.write();
            std::mem::take(&mut state.zwnjs_scratch_space)
        };
        let bytes = scratch.capacity() * std::mem::size_of::<usize>();
        drop(scratch);
        bytes
    }

    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let collection = core_text::font_collection::create_for_all_families();
        let Some(descriptors) = collection.get_descriptors() else {
            return names;
        };
        for descriptor in descriptors.into_iter() {
            names.extend(lenient_font_attributes::family_name(&descriptor));
        }
        if let Ok(fonts_in_memory) = self.0.read().memory_source.all_families() {
            names.extend(fonts_in_memory);
        }
        names
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.0.upgradable_read();
        if let Some(font_id) = lock.font_selections.get(font) {
            Ok(*font_id)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let font_key = FontKey {
                font_family: font.family.clone(),
                font_features: font.features.clone(),
                font_fallbacks: font.fallbacks.clone(),
            };
            let candidates =
                if let Some(candidates) = lock.font_candidates_by_font_key.get(&font_key) {
                    candidates.as_slice()
                } else {
                    let candidates =
                        lock.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
                    lock.font_candidates_by_font_key
                        .insert(font_key.clone(), candidates);
                    lock.font_candidates_by_font_key[&font_key].as_ref()
                };

            let candidates = SmallVec::<[usize; 4]>::from_slice(candidates);
            let font_id = lock.select_font(
                &candidates,
                &Properties {
                    style: font.style.into(),
                    weight: font.weight.into(),
                    stretch: Default::default(),
                },
            )?;
            lock.font_selections.insert(font.clone(), font_id);
            Ok(font_id)
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.0.read().fonts[font_id.0].metrics().into()
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(self.0.read().fonts[font_id.0]
            .typographic_bounds(glyph_id.0)?
            .into())
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.read().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        glyph_id: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.read().rasterize_glyph(glyph_id, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, font_runs)
    }
}

impl MacTextSystemState {
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let fonts = fonts
            .into_iter()
            .map(|bytes| match bytes {
                Cow::Borrowed(embedded_font) => {
                    let data_provider = unsafe {
                        core_graphics::data_provider::CGDataProvider::from_slice(embedded_font)
                    };
                    let font = core_graphics::font::CGFont::from_data_provider(data_provider)
                        .map_err(|()| anyhow!("Could not load an embedded font."))?;
                    let font = font_kit::loaders::core_text::Font::from_core_graphics_font(font);
                    Ok(Handle::from_native(&font))
                }
                Cow::Owned(bytes) => Ok(Handle::from_memory(Arc::new(bytes), 0)),
            })
            .collect::<Result<Vec<_>>>()?;
        self.memory_source.add_fonts(fonts.into_iter())?;
        Ok(())
    }

    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[usize; 4]>> {
        let name = crate::text_system::font_name_with_fallbacks(name, ".AppleSystemUIFont");

        if let Ok(family) = self.memory_source.select_family_by_name(name) {
            return self.load_family_handles(family.fonts(), features, fallbacks, true);
        }

        // Only verified system font identities use deferred registration.
        // Private UI aliases (.AppleSystemUIFont -> .SFNS-Regular), user fonts,
        // and custom configuration retain the existing eager path.
        if features.tag_value_list().is_empty()
            && fallbacks.is_none()
            && let Some(pending) = Self::deferred_system_family(name)?
        {
            return Ok(self.register_candidates(pending));
        }
        let family = self.system_source.select_family_by_name(name)?;
        self.load_family_handles(family.fonts(), features, fallbacks, false)
    }

    fn deferred_system_family(name: &str) -> Result<Option<SmallVec<[FontCandidate; 4]>>> {
        if !matches!(name, "Menlo" | "Helvetica") {
            return Ok(None);
        }
        // Match font-kit's CoreTextSource query and collection ordering exactly;
        // create_for_family uses a different query and mandatory attributes.
        let attributes = CFDictionary::from_CFType_pairs(&[(
            CFString::new("NSFontFamilyAttribute"),
            CFString::new(name).as_CFType(),
        )]);
        let descriptor = core_text::font_descriptor::new_from_attributes(&attributes);
        let collection =
            core_text::font_collection::new_from_descriptors(&CFArray::from_CFTypes(&[descriptor]));
        let descriptors = collection
            .get_descriptors()
            .filter(|descriptors| !descriptors.is_empty())
            .ok_or(font_kit::error::SelectionError::NotFound)?;
        let mut pending = SmallVec::new();
        for descriptor in descriptors.iter() {
            let Some(candidate) = Self::deferred_system_candidate(name, (*descriptor).clone())
            else {
                return Ok(None);
            };
            pending.push(candidate);
        }
        Ok(Some(pending))
    }

    fn deferred_system_candidate(
        name: &str,
        descriptor: CTFontDescriptor,
    ) -> Option<FontCandidate> {
        use core_text::font_descriptor::{kCTFontMatrixAttribute, kCTFontVariationAttribute};

        // A family name alone is insufficient: a user-installed font can have
        // the same family, and private/variable descriptors can normalize their
        // PS names. Limit this optimization to these built-in static faces.
        let (path, names): (&str, &[&str]) = match name {
            "Menlo" => (
                "/System/Library/Fonts/Menlo.ttc",
                &[
                    "Menlo-Regular",
                    "Menlo-Italic",
                    "Menlo-Bold",
                    "Menlo-BoldItalic",
                ],
            ),
            "Helvetica" => (
                "/System/Library/Fonts/Helvetica.ttc",
                &[
                    "Helvetica",
                    "Helvetica-Oblique",
                    "Helvetica-Light",
                    "Helvetica-LightOblique",
                    "Helvetica-Bold",
                    "Helvetica-BoldOblique",
                ],
            ),
            _ => return None,
        };
        let postscript_name = lenient_font_attributes::postscript_name(&descriptor)?;
        if !names.contains(&postscript_name.as_str())
            || lenient_font_attributes::font_path(&descriptor)?.as_path()
                != std::path::Path::new(path)
        {
            return None;
        }
        let attributes = descriptor.attributes();
        if unsafe {
            attributes.find(kCTFontVariationAttribute).is_some()
                || attributes.find(kCTFontMatrixAttribute).is_some()
        } {
            return None;
        }
        Some(FontCandidate {
            properties: None,
            postscript_name,
            state: FontCandidateState::Deferred(descriptor),
            previous_source: None,
        })
    }

    fn load_family_handles(
        &mut self,
        handles: &[Handle],
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
        resident: bool,
    ) -> Result<SmallVec<[usize; 4]>> {
        // A later face can fail to load. Until the whole family succeeds, keep
        // its candidates local so a retry cannot retain a partial family or
        // change the PostScript fallback precedence of registered fonts.
        let mut pending = SmallVec::<[FontCandidate; 4]>::new();
        for handle in handles {
            if let Some(candidate) =
                Self::prepare_font_as(handle.load()?, features, fallbacks, resident)?
            {
                pending.push(candidate);
            }
        }
        Ok(self.register_candidates(pending))
    }

    fn prepare_font(
        font: FontKitFont,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<Option<FontCandidate>> {
        Self::prepare_font_as(font, features, fallbacks, false)
    }

    fn prepare_font_as(
        mut font: FontKitFont,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
        resident: bool,
    ) -> Result<Option<FontCandidate>> {
        apply_features_and_fallbacks(&mut font, features, fallbacks)?;
        // This block contains a precautionary fix to guard against loading fonts
        // that might cause panics due to `.unwrap()`s up the chain.
        {
            // We use the 'm' character for text measurements in various spots
            // (e.g., the editor). However, at time of writing some of those usages
            // will panic if the font has no 'm' glyph.
            //
            // Therefore, we check up front that the font has the necessary glyph.
            let has_m_glyph = font.glyph_for_char('m').is_some();

            // HACK: The 'Segoe Fluent Icons' font does not have an 'm' glyph,
            // but we need to be able to load it for rendering Windows icons in
            // the Storybook (on macOS).
            let is_segoe_fluent_icons = font.full_name() == "Segoe Fluent Icons";

            if !has_m_glyph && !is_segoe_fluent_icons {
                // I spent far too long trying to track down why a font missing the 'm'
                // character wasn't loading. This log statement will hopefully save
                // someone else from suffering the same fate.
                log::warn!(
                    "font '{}' has no 'm' character and was not loaded",
                    font.full_name()
                );
                return Ok(None);
            }
        }

        // We've seen a number of panics in production caused by calling font.properties()
        // which unwraps a downcast to CFNumber. This is an attempt to avoid the panic,
        // and to try and identify the incalcitrant font.
        let traits = font.native_font().all_traits();
        if unsafe {
            !(traits
                .get(kCTFontSymbolicTrait)
                .downcast::<CFNumber>()
                .is_some()
                && traits
                    .get(kCTFontWidthTrait)
                    .downcast::<CFNumber>()
                    .is_some()
                && traits
                    .get(kCTFontWeightTrait)
                    .downcast::<CFNumber>()
                    .is_some()
                && traits
                    .get(kCTFontSlantTrait)
                    .downcast::<CFNumber>()
                    .is_some())
        } {
            log::error!(
                "Failed to read traits for font {:?}",
                font.postscript_name().unwrap()
            );
            return Ok(None);
        }

        let postscript_name = font.postscript_name().unwrap();
        // CTFontCopyFontDescriptor preserves the normalized attributes
        // needed to recreate this exact face, including features and the
        // cascade list. Keep those and CSS properties, not every native
        // font in the family, until this candidate is materialized. Resident
        // add_fonts faces keep their configured font instead.
        let properties = Some(font.properties());
        let state = if resident {
            FontCandidateState::Resident(font)
        } else {
            let native = font.native_font();
            FontCandidateState::Pending {
                descriptor: native.copy_descriptor(),
                point_size: native.pt_size(),
            }
        };
        Ok(Some(FontCandidate {
            properties,
            postscript_name,
            state,
            previous_source: None,
        }))
    }

    fn register_candidates(
        &mut self,
        pending: SmallVec<[FontCandidate; 4]>,
    ) -> SmallVec<[usize; 4]> {
        let mut candidates = SmallVec::new();
        for mut candidate in pending {
            let index = self.font_candidates.len();
            candidate.previous_source = self.font_sources_by_postscript_name.insert(
                candidate.postscript_name.clone(),
                FontSource::Candidate(index),
            );
            self.font_candidates.push(candidate);
            candidates.push(index);
        }
        candidates
    }

    fn prepare_candidate(&mut self, index: usize) -> Result<bool> {
        let descriptor = match &self.font_candidates[index].state {
            FontCandidateState::Deferred(descriptor) => descriptor.clone(),
            FontCandidateState::Rejected => return Ok(false),
            _ => return Ok(true),
        };
        // Use the same size, configuration, glyph query, and native trait
        // conversion as CoreTextSource + load_family_handles. Descriptor traits
        // and character coverage are not substitutes for those checks.
        let native = core_text::font::new_from_descriptor(&descriptor, 16.);
        let font = unsafe { FontKitFont::from_native_font(&native) };
        let resolved = Self::prepare_font(font, &FontFeatures::default(), None)?;
        let candidate = &mut self.font_candidates[index];
        if let Some(resolved) = resolved {
            // Fail without committing an unexpected identity. In particular,
            // never silently relabel a native font or change a newer family's
            // PostScript precedence after this candidate was registered.
            anyhow::ensure!(
                resolved.postscript_name == candidate.postscript_name,
                "system font identity changed from {} to {}",
                candidate.postscript_name,
                resolved.postscript_name,
            );
            candidate.properties = resolved.properties;
            candidate.state = resolved.state;
            Ok(true)
        } else {
            candidate.state = FontCandidateState::Rejected;
            // A deferred face is only a provisional fallback source. Removing
            // an ineligible later face restores the previous registered source;
            // links through it remain usable by still-later candidates.
            if self
                .font_sources_by_postscript_name
                .get(&candidate.postscript_name)
                == Some(&FontSource::Candidate(index))
            {
                if let Some(previous) = candidate.previous_source {
                    self.font_sources_by_postscript_name
                        .insert(candidate.postscript_name.clone(), previous);
                } else {
                    self.font_sources_by_postscript_name
                        .remove(&candidate.postscript_name);
                }
            }
            Ok(false)
        }
    }

    fn select_font(&mut self, candidates: &[usize], query: &Properties) -> Result<FontId> {
        let mut eligible = SmallVec::<[usize; 4]>::new();
        let mut properties = SmallVec::<[Properties; 4]>::new();
        for &candidate in candidates {
            if self.prepare_candidate(candidate)? {
                let actual = self.font_candidates[candidate].properties.unwrap();
                // CSS matching prefers exact stretch, then exact style, then
                // exact weight, and finally the earliest family index. A later
                // face cannot beat this full match. Without one, inspect every
                // face and let the unchanged font-kit matcher decide.
                if actual == *query {
                    return self.materialize_font(candidate);
                }
                eligible.push(candidate);
                properties.push(actual);
            }
        }
        let selected = font_kit::matching::find_best_match(&properties, query)?;
        self.materialize_font(eligible[selected])
    }

    fn materialize_font(&mut self, candidate: usize) -> Result<FontId> {
        if !self.prepare_candidate(candidate)? {
            return Err(font_kit::error::SelectionError::NotFound.into());
        }
        let candidate = &mut self.font_candidates[candidate];
        let font = match &candidate.state {
            FontCandidateState::Loaded(font_id) => return Ok(*font_id),
            FontCandidateState::Pending {
                descriptor,
                point_size,
            } => {
                let native = core_text::font::new_from_descriptor(descriptor, *point_size);
                // The native font was created above and is alive for this call.
                unsafe { FontKitFont::from_native_font(&native) }
            }
            FontCandidateState::Resident(font) => font.clone(),
            FontCandidateState::Deferred(_) | FontCandidateState::Rejected => unreachable!(),
        };
        let font_id = FontId(self.fonts.len());
        self.fonts.push(font);
        self.postscript_names_by_font_id
            .insert(font_id, candidate.postscript_name.clone());
        // The stable font table now owns the live font. Retire the candidate's
        // descriptor or resident font; this candidate will only use its ID again.
        candidate.state = FontCandidateState::Loaded(font_id);
        // Only loading a family changes PostScript fallback precedence. Using
        // an older candidate must not override a more recently loaded family.
        Ok(font_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(self.fonts[font_id.0].advance(glyph_id.0)?.into())
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.fonts[font_id.0].glyph_for_char(ch).map(GlyphId)
    }

    fn id_for_native_font(&mut self, requested_font: CTFont) -> FontId {
        let postscript_name = requested_font.postscript_name();
        let mut source = self
            .font_sources_by_postscript_name
            .get(&postscript_name)
            .copied();
        while let Some(current) = source {
            match current {
                FontSource::Candidate(candidate) => {
                    match self.materialize_font(candidate) {
                        Ok(font_id) => return font_id,
                        Err(error) => {
                            if !matches!(
                                self.font_candidates[candidate].state,
                                FontCandidateState::Rejected
                            ) {
                                log::error!(
                                    "Failed to load fallback font {postscript_name}: {error}"
                                );
                            }
                        }
                    }
                    source = self.font_candidates[candidate].previous_source;
                }
                FontSource::Loaded(font_id) => return font_id,
            }
        }
        let font_id = FontId(self.fonts.len());
        self.font_sources_by_postscript_name
            .insert(postscript_name.clone(), FontSource::Loaded(font_id));
        self.postscript_names_by_font_id
            .insert(font_id, postscript_name);
        self.fonts
            .push(font_kit::font::Font::from_core_graphics_font(
                requested_font.copy_to_CGFont(),
            ));
        font_id
    }

    fn is_emoji(&self, font_id: FontId) -> bool {
        self.postscript_names_by_font_id
            .get(&font_id)
            .is_some_and(|postscript_name| {
                postscript_name == "AppleColorEmoji" || postscript_name == ".AppleColorEmojiUI"
            })
    }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.fonts[params.font_id.0];
        let scale = Transform2F::from_scale(params.scale_factor);
        Ok(font
            .raster_bounds(
                params.glyph_id.0,
                params.font_size.into(),
                scale,
                HintingOptions::None,
                font_kit::canvas::RasterizationOptions::GrayscaleAa,
            )?
            .into())
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        } else {
            // Add an extra pixel when the subpixel variant isn't zero to make room for anti-aliasing.
            let mut bitmap_size = glyph_bounds.size;
            if params.subpixel_variant.x > 0 {
                bitmap_size.width += DevicePixels(1);
            }
            if params.subpixel_variant.y > 0 {
                bitmap_size.height += DevicePixels(1);
            }
            let bitmap_size = bitmap_size;

            let mut bytes;
            let cx;
            if params.is_emoji {
                bytes = vec![0; bitmap_size.width.0 as usize * 4 * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize * 4,
                    &CGColorSpace::create_device_rgb(),
                    kCGImageAlphaPremultipliedLast,
                );
            } else {
                bytes = vec![0; bitmap_size.width.0 as usize * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize,
                    &CGColorSpace::create_device_gray(),
                    kCGImageAlphaOnly,
                );
            }

            // Move the origin to bottom left and account for scaling, this
            // makes drawing text consistent with the font-kit's raster_bounds.
            cx.translate(
                -glyph_bounds.origin.x.0 as CGFloat,
                (glyph_bounds.origin.y.0 + glyph_bounds.size.height.0) as CGFloat,
            );
            cx.scale(
                params.scale_factor as CGFloat,
                params.scale_factor as CGFloat,
            );

            let subpixel_shift = params
                .subpixel_variant
                .map(|v| v as f32 / SUBPIXEL_VARIANTS_X as f32);
            cx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
            cx.set_gray_fill_color(0.0, 1.0);
            cx.set_allows_antialiasing(true);
            cx.set_should_antialias(true);
            cx.set_allows_font_subpixel_positioning(true);
            cx.set_should_subpixel_position_fonts(true);
            cx.set_allows_font_subpixel_quantization(false);
            cx.set_should_subpixel_quantize_fonts(false);
            self.fonts[params.font_id.0]
                .native_font()
                .clone_with_font_size(f32::from(params.font_size) as CGFloat)
                .draw_glyphs(
                    &[params.glyph_id.0 as CGGlyph],
                    &[CGPoint::new(
                        (subpixel_shift.x / params.scale_factor) as CGFloat,
                        (subpixel_shift.y / params.scale_factor) as CGFloat,
                    )],
                    cx,
                );

            if params.is_emoji {
                // Convert from RGBA with premultiplied alpha to BGRA with straight alpha.
                for pixel in bytes.chunks_exact_mut(4) {
                    swap_rgba_pa_to_bgra(pixel);
                }
            }

            Ok((bitmap_size, bytes))
        }
    }

    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        const ZWNJ: char = '\u{200C}';
        const ZWNJ_STR: &str = "\u{200C}";
        const ZWNJ_SIZE_16: usize = ZWNJ.len_utf16();

        self.zwnjs_scratch_space.clear();
        // Construct the attributed string, converting UTF8 ranges to UTF16 ranges.
        let mut string = CFMutableAttributedString::new();
        let mut max_ascent = 0.0f32;
        let mut max_descent = 0.0f32;

        {
            let mut ix_converter = StringIndexConverter::new(&text);
            let mut last_font_run = None;
            for run in font_runs {
                let text = &text[ix_converter.utf8_ix..][..run.len];
                // if the fonts are the same, we need to disconnect the text with a ZWNJ
                // to prevent core text from forming ligatures between them
                let needs_zwnj = last_font_run.replace(run.font_id) == Some(run.font_id);

                let utf16_start = string.char_len(); // insert at end of string
                ix_converter.advance_to_utf8_ix(ix_converter.utf8_ix + run.len);

                // note: replace_str may silently ignore codepoints it dislikes (e.g., BOM at start of string)
                string.replace_str(&CFString::new(text), CFRange::init(utf16_start, 0));
                if needs_zwnj {
                    let zwnjs_pos = string.char_len();
                    self.zwnjs_scratch_space.push(zwnjs_pos as usize);
                    string.replace_str(
                        &CFString::from_static_string(ZWNJ_STR),
                        CFRange::init(zwnjs_pos, 0),
                    );
                }
                let utf16_end = string.char_len();

                let cf_range = CFRange::init(utf16_start, utf16_end - utf16_start);
                let font = &self.fonts[run.font_id.0];

                let font_metrics = font.metrics();
                let font_scale = font_size.0 / font_metrics.units_per_em as f32;
                max_ascent = max_ascent.max(font_metrics.ascent * font_scale);
                max_descent = max_descent.max(-font_metrics.descent * font_scale);

                unsafe {
                    string.set_attribute(
                        cf_range,
                        kCTFontAttributeName,
                        &font.native_font().clone_with_font_size(font_size.into()),
                    );
                }
            }
        }
        // Retrieve the glyphs from the shaped line, converting UTF16 offsets to UTF8 offsets.
        let line = CTLine::new_with_attributed_string(string.as_concrete_TypeRef());
        let glyph_runs = line.glyph_runs();
        let mut runs = <Vec<ShapedRun>>::with_capacity(glyph_runs.len() as usize);
        let mut ix_converter = StringIndexConverter::new(text);
        for run in glyph_runs.into_iter() {
            let attributes = run.attributes().unwrap();
            let font = unsafe {
                attributes
                    .get(kCTFontAttributeName)
                    .downcast::<CTFont>()
                    .unwrap()
            };
            let font_id = self.id_for_native_font(font);

            let mut glyphs = match runs.last_mut() {
                Some(run) if run.font_id == font_id => &mut run.glyphs,
                _ => {
                    runs.push(ShapedRun {
                        font_id,
                        glyphs: Vec::with_capacity(run.glyph_count().try_into().unwrap_or(0)),
                    });
                    &mut runs.last_mut().unwrap().glyphs
                }
            };
            for ((&glyph_id, position), &glyph_utf16_ix) in run
                .glyphs()
                .iter()
                .zip(run.positions().iter())
                .zip(run.string_indices().iter())
            {
                let mut glyph_utf16_ix = usize::try_from(glyph_utf16_ix).unwrap();
                let r = self
                    .zwnjs_scratch_space
                    .binary_search_by(|&it| it.cmp(&glyph_utf16_ix));
                match r {
                    // this glyph is a ZWNJ, skip it
                    Ok(_) => continue,
                    // adjust the index to account for the ZWNJs we've inserted
                    Err(idx) => glyph_utf16_ix -= idx * ZWNJ_SIZE_16,
                }
                if ix_converter.utf16_ix > glyph_utf16_ix {
                    // We cannot reuse current index converter, as it can only seek forward. Restart the search.
                    ix_converter = StringIndexConverter::new(text);
                }
                ix_converter.advance_to_utf16_ix(glyph_utf16_ix);
                glyphs.push(ShapedGlyph {
                    id: GlyphId(glyph_id as u32),
                    position: point(position.x as f32, position.y as f32).map(px),
                    index: ix_converter.utf8_ix,
                    is_emoji: self.is_emoji(font_id),
                });
            }
        }
        let typographic_bounds = line.get_typographic_bounds();
        LineLayout {
            runs,
            font_size,
            width: typographic_bounds.width.into(),
            ascent: max_ascent.into(),
            descent: max_descent.into(),
            len: text.len(),
        }
    }
}

#[derive(Debug, Clone)]
struct StringIndexConverter<'a> {
    text: &'a str,
    /// Index in UTF-8 bytes
    utf8_ix: usize,
    /// Index in UTF-16 code units
    utf16_ix: usize,
}

impl<'a> StringIndexConverter<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            utf8_ix: 0,
            utf16_ix: 0,
        }
    }

    fn advance_to_utf8_ix(&mut self, utf8_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf8_ix + ix >= utf8_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }

    fn advance_to_utf16_ix(&mut self, utf16_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf16_ix >= utf16_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }
}

impl From<Metrics> for FontMetrics {
    fn from(metrics: Metrics) -> Self {
        FontMetrics {
            units_per_em: metrics.units_per_em,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.line_gap,
            underline_position: metrics.underline_position,
            underline_thickness: metrics.underline_thickness,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: metrics.bounding_box.into(),
        }
    }
}

impl From<RectF> for Bounds<f32> {
    fn from(rect: RectF) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<RectI> for Bounds<DevicePixels> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
            size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
        }
    }
}

impl From<Vector2I> for Size<DevicePixels> {
    fn from(value: Vector2I) -> Self {
        size(value.x().into(), value.y().into())
    }
}

impl From<RectI> for Bounds<i32> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<Point<u32>> for Vector2I {
    fn from(size: Point<u32>) -> Self {
        Vector2I::new(size.x as i32, size.y as i32)
    }
}

impl From<Vector2F> for Size<f32> {
    fn from(vec: Vector2F) -> Self {
        size(vec.x(), vec.y())
    }
}

impl From<FontWeight> for FontkitWeight {
    fn from(value: FontWeight) -> Self {
        FontkitWeight(value.0)
    }
}

impl From<FontStyle> for FontkitStyle {
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => FontkitStyle::Normal,
            FontStyle::Italic => FontkitStyle::Italic,
            FontStyle::Oblique => FontkitStyle::Oblique,
        }
    }
}

// Some fonts may have no attributes despite `core_text` requiring them (and panicking).
// This is the same version as `core_text` has without `expect` calls.
mod lenient_font_attributes {
    use core_foundation::{
        base::{CFType, TCFType},
        string::{CFString, CFStringRef},
        url::CFURL,
    };
    use core_text::font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCopyAttribute, kCTFontFamilyNameAttribute,
        kCTFontNameAttribute, kCTFontURLAttribute,
    };

    pub fn family_name(descriptor: &CTFontDescriptor) -> Option<String> {
        unsafe { string_attribute(descriptor, kCTFontFamilyNameAttribute) }
    }

    pub fn postscript_name(descriptor: &CTFontDescriptor) -> Option<String> {
        unsafe { string_attribute(descriptor, kCTFontNameAttribute) }
    }

    pub fn font_path(descriptor: &CTFontDescriptor) -> Option<std::path::PathBuf> {
        unsafe { attribute(descriptor, kCTFontURLAttribute) }
            .and_then(|value| value.downcast::<CFURL>())
            .and_then(|url| url.to_path())
    }

    fn string_attribute(descriptor: &CTFontDescriptor, key: CFStringRef) -> Option<String> {
        attribute(descriptor, key)?
            .downcast::<CFString>()
            .map(|value| value.to_string())
    }

    fn attribute(descriptor: &CTFontDescriptor, key: CFStringRef) -> Option<CFType> {
        unsafe {
            let value = CTFontDescriptorCopyAttribute(descriptor.as_concrete_TypeRef(), key);
            (!value.is_null()).then(|| CFType::wrap_under_create_rule(value))
        }
    }
}

#[cfg(feature = "test-support")]
#[allow(missing_docs)]
pub(crate) mod fastab_resource_fixtures {
    use super::*;

    fn eager_family(request: &Font) -> Vec<FontKitFont> {
        SystemSource::new()
            .select_family_by_name(&request.family)
            .unwrap()
            .fonts()
            .iter()
            .map(|handle| {
                let mut font = handle.load().unwrap();
                apply_features_and_fallbacks(
                    &mut font,
                    &request.features,
                    request.fallbacks.as_ref(),
                )
                .unwrap();
                assert!(font.glyph_for_char('m').is_some());
                font
            })
            .collect()
    }

    fn eager_text_system(fonts: Vec<FontKitFont>) -> MacTextSystem {
        let system = MacTextSystem::new();
        {
            let mut state = system.0.write();
            for font in fonts {
                let font_id = FontId(state.fonts.len());
                let name = font.postscript_name().unwrap();
                state
                    .font_sources_by_postscript_name
                    .insert(name.clone(), FontSource::Loaded(font_id));
                state.postscript_names_by_font_id.insert(font_id, name);
                state.fonts.push(font);
            }
        }
        system
    }

    fn assert_same_layout(
        expected: &LineLayout,
        actual: &LineLayout,
        expected_fonts: &MacTextSystemState,
        actual_fonts: &MacTextSystemState,
    ) {
        assert_eq!(expected.font_size, actual.font_size);
        assert_eq!(expected.width, actual.width);
        assert_eq!(expected.ascent, actual.ascent);
        assert_eq!(expected.descent, actual.descent);
        assert_eq!(expected.len, actual.len);
        assert_eq!(expected.runs.len(), actual.runs.len());
        for (expected, actual) in expected.runs.iter().zip(&actual.runs) {
            assert_eq!(
                expected_fonts.postscript_names_by_font_id[&expected.font_id],
                actual_fonts.postscript_names_by_font_id[&actual.font_id],
            );
            assert_eq!(expected.glyphs.len(), actual.glyphs.len());
            for (expected, actual) in expected.glyphs.iter().zip(&actual.glyphs) {
                assert_eq!(expected.id, actual.id);
                assert_eq!(expected.position, actual.position);
                assert_eq!(expected.index, actual.index);
                assert_eq!(expected.is_emoji, actual.is_emoji);
            }
        }
    }

    pub fn native_font_families_materialize_only_selected_faces() {
        objc::rc::autoreleasepool(|| {
            for family in [".AppleSystemUIFont", "Helvetica", "Menlo"] {
                let system = MacTextSystem::new();
                let mut request = crate::font(family);
                let eager = eager_family(&request);
                let properties = eager
                    .iter()
                    .map(FontKitFont::properties)
                    .collect::<Vec<_>>();
                assert!(eager.len() > 1);
                let first_id = system.font_id(&request).unwrap();
                assert_eq!(system.0.read().fonts.len(), 1);
                assert_eq!(system.0.read().font_candidates.len(), eager.len());
                assert_eq!(system.font_id(&request).unwrap(), first_id);

                let mut selected = HashMap::default();
                for style in [FontStyle::Normal, FontStyle::Italic, FontStyle::Oblique] {
                    for weight in [400., 100., 350., 450., 500., 550., 700., 900.] {
                        request.style = style;
                        request.weight = FontWeight(weight);
                        let expected_index = font_kit::matching::find_best_match(
                            &properties,
                            &Properties {
                                style: style.into(),
                                weight: request.weight.into(),
                                stretch: Default::default(),
                            },
                        )
                        .unwrap();
                        let id = system.font_id(&request).unwrap();
                        let state = system.0.read();
                        assert_eq!(
                            state.fonts[id.0].postscript_name(),
                            eager[expected_index].postscript_name(),
                        );
                        assert_eq!(state.fonts[id.0].properties(), properties[expected_index]);
                        if let Some(previous) = selected.insert(expected_index, id) {
                            assert_eq!(id, previous);
                        }
                        assert_eq!(state.fonts.len(), selected.len());
                    }
                }
            }
        });
    }

    pub fn native_font_descriptors_preserve_attributes_and_shaping() {
        use core_text::font_descriptor::{
            kCTFontCascadeListAttribute, kCTFontFeatureSettingsAttribute, kCTFontMatrixAttribute,
            kCTFontVariationAttribute,
        };

        objc::rc::autoreleasepool(|| {
            for family in [".AppleSystemUIFont", "Helvetica", "Menlo"] {
                let mut request = crate::font(family);
                request.features = FontFeatures::disable_ligatures();
                request.fallbacks = Some(FontFallbacks::from_fonts(vec!["Menlo".into()]));
                let eager = eager_text_system(eager_family(&request));
                let lazy = MacTextSystem::new();
                let candidates = lazy
                    .0
                    .write()
                    .load_family(family, &request.features, request.fallbacks.as_ref())
                    .unwrap();
                assert!(lazy.0.read().fonts.is_empty());
                assert_eq!(candidates.len(), eager.0.read().fonts.len());
                for (index, candidate) in candidates.into_iter().enumerate() {
                    let id = lazy.0.write().materialize_font(candidate).unwrap();
                    {
                        let expected = &eager.0.read().fonts[index];
                        let actual = &lazy.0.read().fonts[id.0];
                        assert_eq!(expected.postscript_name(), actual.postscript_name());
                        assert_eq!(expected.properties(), actual.properties());
                        let expected = expected.native_font().copy_descriptor().attributes();
                        let actual = actual.native_font().copy_descriptor().attributes();
                        for attribute in unsafe {
                            [
                                kCTFontFeatureSettingsAttribute,
                                kCTFontCascadeListAttribute,
                                kCTFontMatrixAttribute,
                                kCTFontVariationAttribute,
                            ]
                        } {
                            assert_eq!(expected.find(attribute), actual.find(attribute));
                        }
                    }
                    let text = "office ffi 中文 😀 a\u{feff}b";
                    let expected = eager.layout_line(
                        text,
                        px(17.),
                        &[FontRun {
                            font_id: FontId(index),
                            len: text.len(),
                        }],
                    );
                    let actual = lazy.layout_line(
                        text,
                        px(17.),
                        &[FontRun {
                            font_id: id,
                            len: text.len(),
                        }],
                    );
                    assert_same_layout(&expected, &actual, &eager.0.read(), &lazy.0.read());
                }
            }

            // add_fonts also supplies native CoreGraphics-backed handles via
            // MemSource. Helvetica data would resolve by name either way, so
            // also pin that these faces keep their font instead of a descriptor.
            let native = core_text::font::new_from_name("Helvetica", 16.).unwrap();
            let memory_font = FontKitFont::from_core_graphics_font(native.copy_to_CGFont());
            let family = memory_font.family_name();
            let system = MacTextSystem::new();
            system
                .0
                .write()
                .memory_source
                .add_fonts([Handle::from_native(&memory_font)].into_iter())
                .unwrap();
            let mut state = system.0.write();
            let candidates = state
                .load_family(&family, &FontFeatures::disable_ligatures(), None)
                .unwrap();
            assert!(!candidates.is_empty());
            assert!(candidates.iter().all(|&index| matches!(
                state.font_candidates[index].state,
                FontCandidateState::Resident(_)
            )));
            let id = state.select_font(&candidates, &Properties::new()).unwrap();
            assert!(candidates.iter().any(|&index| matches!(
                state.font_candidates[index].state,
                FontCandidateState::Loaded(loaded) if loaded == id
            )));
            let actual = state.fonts[id.0].clone();
            drop(state);
            assert_eq!(actual.postscript_name(), memory_font.postscript_name());
            assert_eq!(actual.glyph_for_char('m'), memory_font.glyph_for_char('m'));
            assert_eq!(actual.properties(), memory_font.properties());
        });
    }

    pub fn system_font_enumeration_stops_after_first_exact_match() {
        objc::rc::autoreleasepool(|| {
            for family in ["Menlo", "Helvetica"] {
                let request = crate::font(family);
                let system = MacTextSystem::new();
                let candidates = system
                    .0
                    .write()
                    .load_family(family, &request.features, None)
                    .unwrap();
                {
                    let state = system.0.read();
                    assert!(state.fonts.is_empty());
                    assert!(candidates.len() > 1);
                    assert!(candidates.iter().all(|&index| {
                        state.font_candidates[index].properties.is_none()
                            && matches!(
                                state.font_candidates[index].state,
                                FontCandidateState::Deferred(_)
                            )
                    }));
                }
                {
                    use core_foundation::url::CFURL;
                    use core_text::font_descriptor::{
                        kCTFontFamilyNameAttribute, kCTFontNameAttribute, kCTFontURLAttribute,
                    };
                    let state = system.0.read();
                    let FontCandidateState::Deferred(descriptor) =
                        &state.font_candidates[candidates[0]].state
                    else {
                        unreachable!();
                    };
                    let postscript_name =
                        lenient_font_attributes::postscript_name(descriptor).unwrap();
                    let user_path =
                        std::path::PathBuf::from(format!("/Library/Fonts/{family}.ttc"));
                    // Matched descriptors can derive family metadata from their
                    // URL. Preserve identity explicitly when replacing that URL
                    // so the guard rejects the source, not missing attributes.
                    let attributes = CFDictionary::from_CFType_pairs(&[
                        (
                            unsafe { CFString::wrap_under_get_rule(kCTFontFamilyNameAttribute) },
                            CFString::new(family).as_CFType(),
                        ),
                        (
                            unsafe { CFString::wrap_under_get_rule(kCTFontNameAttribute) },
                            CFString::new(&postscript_name).as_CFType(),
                        ),
                        (
                            unsafe { CFString::wrap_under_get_rule(kCTFontURLAttribute) },
                            CFURL::from_path(&user_path, false).unwrap().as_CFType(),
                        ),
                    ]);
                    let user_font = descriptor
                        .create_copy_with_attributes(attributes.into_untyped())
                        .unwrap();
                    assert_eq!(
                        lenient_font_attributes::family_name(&user_font).as_deref(),
                        Some(family)
                    );
                    assert_eq!(
                        lenient_font_attributes::postscript_name(&user_font),
                        Some(postscript_name)
                    );
                    assert_eq!(
                        lenient_font_attributes::font_path(&user_font),
                        Some(user_path)
                    );
                    assert!(
                        MacTextSystemState::deferred_system_candidate(family, user_font).is_none()
                    );
                }
                // Independent eager CoreTextSource reference, including every
                // native face. Enumeration above retained only raw descriptors.
                let eager = eager_family(&request);
                assert_eq!(candidates.len(), eager.len());
                let query = Properties::new();
                let first_exact = eager
                    .iter()
                    .position(|font| font.properties() == query)
                    .unwrap();
                assert!(
                    first_exact + 1 < eager.len(),
                    "{family} must leave uninitialized faces"
                );
                let id = system.0.write().select_font(&candidates, &query).unwrap();
                {
                    let state = system.0.read();
                    assert_eq!(state.fonts.len(), 1);
                    assert_eq!(
                        state.fonts[id.0].postscript_name(),
                        eager[first_exact].postscript_name()
                    );
                    for (position, &index) in candidates.iter().enumerate() {
                        assert_eq!(
                            state.font_candidates[index].postscript_name,
                            eager[position].postscript_name().unwrap()
                        );
                        assert_eq!(
                            matches!(
                                state.font_candidates[index].state,
                                FontCandidateState::Deferred(_)
                            ),
                            position > first_exact
                        );
                    }
                }
                assert_eq!(
                    system.0.write().select_font(&candidates, &query).unwrap(),
                    id
                );
                let eager_system = eager_text_system(eager.clone());
                let text = "office ffi 中文 😀 a\u{feff}b";
                let expected = eager_system.layout_line(
                    text,
                    px(17.),
                    &[FontRun {
                        font_id: FontId(first_exact),
                        len: text.len(),
                    }],
                );
                let actual = system.layout_line(
                    text,
                    px(17.),
                    &[FontRun {
                        font_id: id,
                        len: text.len(),
                    }],
                );
                assert_same_layout(&expected, &actual, &eager_system.0.read(), &system.0.read());

                // A non-exact request must inspect the complete family before
                // invoking the same CSS matcher, including stretch preferences.
                let properties = eager
                    .iter()
                    .map(FontKitFont::properties)
                    .collect::<Vec<_>>();
                for style in [
                    FontkitStyle::Normal,
                    FontkitStyle::Italic,
                    FontkitStyle::Oblique,
                ] {
                    for stretch in [0.75, 1.0, 1.25] {
                        let query = Properties {
                            style,
                            weight: FontkitWeight(456.75),
                            stretch: font_kit::properties::Stretch(stretch),
                        };
                        assert!(!properties.contains(&query));
                        let expected =
                            font_kit::matching::find_best_match(&properties, &query).unwrap();
                        let actual = system.0.write().select_font(&candidates, &query).unwrap();
                        let state = system.0.read();
                        assert_eq!(
                            state.fonts[actual.0].postscript_name(),
                            eager[expected].postscript_name()
                        );
                        assert!(candidates.iter().all(|&index| !matches!(
                            state.font_candidates[index].state,
                            FontCandidateState::Deferred(_)
                        )));
                    }
                }
                system.release_idle_caches();
                assert_eq!(
                    system
                        .0
                        .write()
                        .select_font(&candidates, &Properties::new())
                        .unwrap(),
                    id
                );
            }

            // This alias has different descriptor and native PostScript names.
            // It must retain the eager registration behavior, as must explicit
            // feature/cascade configuration even on otherwise eligible families.
            for (family, features, fallbacks) in [
                (".SystemUIFont", FontFeatures::default(), None),
                ("Menlo", FontFeatures::disable_ligatures(), None),
                (
                    "Helvetica",
                    FontFeatures::default(),
                    Some(FontFallbacks::from_fonts(vec!["Menlo".into()])),
                ),
            ] {
                let system = MacTextSystem::new();
                let candidates = system
                    .0
                    .write()
                    .load_family(family, &features, fallbacks.as_ref())
                    .unwrap();
                assert!(!candidates.is_empty());
                let state = system.0.read();
                assert!(candidates.iter().all(|&index| matches!(
                    state.font_candidates[index].state,
                    FontCandidateState::Pending { .. }
                )));
            }

            // A same-family MemSource entry must still shadow system fonts,
            // even for the default configuration eligible for deferred loading.
            let native = core_text::font::new_from_name("Helvetica", 16.).unwrap();
            let memory_font = FontKitFont::from_core_graphics_font(native.copy_to_CGFont());
            let system = MacTextSystem::new();
            system
                .0
                .write()
                .memory_source
                .add_fonts([Handle::from_native(&memory_font)].into_iter())
                .unwrap();
            let id = system
                .font_id(&crate::font(memory_font.family_name()))
                .unwrap();
            let state = system.0.read();
            assert_eq!(state.font_candidates.len(), 1);
            assert!(
                matches!(state.font_candidates[0].state, FontCandidateState::Loaded(loaded) if loaded == id)
            );
            assert_eq!(
                state.fonts[id.0].postscript_name(),
                memory_font.postscript_name()
            );
            assert_eq!(state.fonts[id.0].properties(), memory_font.properties());
        });
    }

    pub fn deferred_font_candidates_preserve_matching_and_fallback_precedence() {
        use core_foundation::{
            base::kCFAllocatorDefault,
            characterset::{CFCharacterSet, CFCharacterSetCreateWithCharactersInString},
        };
        use core_text::font_descriptor::kCTFontCharacterSetAttribute;

        objc::rc::autoreleasepool(|| {
            let native = core_text::font::new_from_name("Helvetica", 16.).unwrap();
            let descriptor = native.copy_descriptor();
            let empty = unsafe {
                CFCharacterSet::wrap_under_create_rule(CFCharacterSetCreateWithCharactersInString(
                    kCFAllocatorDefault,
                    CFString::new("").as_concrete_TypeRef(),
                ))
            };
            // CoreText documents that this attribute restricts the actual
            // character set. This is a real native font with the same PS name
            // and no 'm', not a mocked eligibility failure or altered CSS trait.
            let attributes = CFDictionary::from_CFType_pairs(&[(
                unsafe { CFString::wrap_under_get_rule(kCTFontCharacterSetAttribute) },
                empty.as_CFType(),
            )]);
            let restricted = descriptor
                .create_copy_with_attributes(attributes.into_untyped())
                .unwrap();
            let restricted_native = core_text::font::new_from_descriptor(&restricted, 16.);
            let mut restricted_font = unsafe { FontKitFont::from_native_font(&restricted_native) };
            apply_features_and_fallbacks(&mut restricted_font, &FontFeatures::default(), None)
                .unwrap();
            assert_eq!(
                restricted_font.postscript_name().unwrap(),
                native.postscript_name()
            );
            assert_eq!(restricted_font.glyph_for_char('m'), None);
            assert!(
                MacTextSystemState::prepare_font(restricted_font, &FontFeatures::default(), None)
                    .unwrap()
                    .is_none()
            );

            let deferred = |descriptor: CTFontDescriptor| FontCandidate {
                properties: None,
                postscript_name: native.postscript_name(),
                state: FontCandidateState::Deferred(descriptor),
                previous_source: None,
            };
            let system = MacTextSystem::new();
            let mut state = system.0.write();
            let older = state
                .load_family("Helvetica", &FontFeatures::disable_ligatures(), None)
                .unwrap();
            let name = native.postscript_name();
            let old_candidate = *older
                .iter()
                .find(|&&index| state.font_candidates[index].postscript_name == name)
                .unwrap();
            let old_id = state.materialize_font(old_candidate).unwrap();
            let previous = state.font_sources_by_postscript_name[&name];
            let rejected = state.register_candidates(SmallVec::from_vec(vec![
                deferred(restricted.clone()),
                deferred(restricted.clone()),
            ]));
            assert_eq!(state.id_for_native_font(native.clone()), old_id);
            assert_eq!(state.font_sources_by_postscript_name[&name], previous);
            assert_eq!(state.fonts.len(), 1);
            assert!(rejected.iter().all(|&index| matches!(
                state.font_candidates[index].state,
                FontCandidateState::Rejected
            )));
            assert_eq!(state.id_for_native_font(native.clone()), old_id);

            // Rejection during CSS selection must also continue in original
            // order; rejecting an older node must not disturb the newer head.
            let retry = state.register_candidates(SmallVec::from_vec(vec![
                deferred(restricted),
                deferred(descriptor),
            ]));
            let newest = FontSource::Candidate(retry[1]);
            let selected = state.select_font(&retry, &Properties::new()).unwrap();
            assert!(matches!(
                state.font_candidates[retry[0]].state,
                FontCandidateState::Rejected
            ));
            assert_eq!(state.font_sources_by_postscript_name[&name], newest);
            assert_ne!(selected, old_id);
            assert_eq!(state.fonts.len(), 2);
            assert_eq!(state.id_for_native_font(native.clone()), selected);
            assert_eq!(state.materialize_font(old_candidate).unwrap(), old_id);
            assert_eq!(state.id_for_native_font(native), selected);

            // Private aliases are excluded at the family gate. Exercise the
            // second identity guard with CoreText's real normalization too:
            // no native ID, CSS properties, or PS source is partially changed.
            let alias = core_text::font_descriptor::new_from_postscript_name(&CFString::new(
                ".AppleSystemUIFont",
            ));
            let index = state.register_candidates(SmallVec::from_vec(vec![FontCandidate {
                properties: None,
                postscript_name: ".AppleSystemUIFont".into(),
                state: FontCandidateState::Deferred(alias),
                previous_source: None,
            }]))[0];
            let font_count = state.fonts.len();
            let sources = state.font_sources_by_postscript_name.clone();
            let names = state.postscript_names_by_font_id.clone();
            for _ in 0..2 {
                assert!(state.prepare_candidate(index).is_err());
                assert!(state.font_candidates[index].properties.is_none());
                assert!(matches!(
                    state.font_candidates[index].state,
                    FontCandidateState::Deferred(_)
                ));
                assert_eq!(state.fonts.len(), font_count);
                assert_eq!(state.font_sources_by_postscript_name, sources);
                assert_eq!(state.postscript_names_by_font_id, names);
            }
        });
    }

    pub fn native_fallback_preserves_candidate_precedence_and_font_ids() {
        objc::rc::autoreleasepool(|| {
            let system = MacTextSystem::new();
            let mut state = system.0.write();
            let older = state
                .load_family("Helvetica", &FontFeatures::default(), None)
                .unwrap();
            let newer = state
                .load_family("Helvetica", &FontFeatures::disable_ligatures(), None)
                .unwrap();
            assert!(state.fonts.is_empty());
            let old_candidate = older[0];
            let name = state.font_candidates[old_candidate].postscript_name.clone();
            let new_candidate = *newer
                .iter()
                .rev()
                .find(|&&candidate| state.font_candidates[candidate].postscript_name == name)
                .unwrap();
            let old_id = state.materialize_font(old_candidate).unwrap();
            let old_native = state.fonts[old_id.0].native_font();
            assert_eq!(state.fonts.len(), 1);
            // A shaped native font with this PS name must use the most recently
            // loaded family's configured candidate, even if it was never selected.
            let fallback_id = state.id_for_native_font(old_native.clone());
            assert_ne!(fallback_id, old_id);
            assert_eq!(fallback_id, state.materialize_font(new_candidate).unwrap());
            assert_eq!(state.fonts.len(), 2);
            assert_eq!(state.materialize_font(old_candidate).unwrap(), old_id);
            assert_eq!(state.id_for_native_font(old_native), fallback_id);
            assert_eq!(state.fonts.len(), 2);

            let emoji = core_text::font::new_from_name("AppleColorEmoji", 16.).unwrap();
            let emoji_id = state.id_for_native_font(emoji.clone());
            assert_eq!(state.id_for_native_font(emoji), emoji_id);
            assert!(state.is_emoji(emoji_id));
            assert_eq!(state.fonts.len(), 3);
            drop(state);
            system.release_idle_caches();
            let mut state = system.0.write();
            assert_eq!(state.materialize_font(old_candidate).unwrap(), old_id);
            assert_eq!(state.materialize_font(new_candidate).unwrap(), fallback_id);
            assert_eq!(state.fonts.len(), 3);
        });
    }

    pub fn materialized_font_candidates_release_descriptors() {
        objc::rc::autoreleasepool(|| {
            let system = MacTextSystem::new();
            let mut state = system.0.write();
            let candidates = state
                .load_family("Helvetica", &FontFeatures::disable_ligatures(), None)
                .unwrap();
            assert!(candidates.len() > 1);
            assert!(candidates.iter().all(|&candidate| matches!(
                state.font_candidates[candidate].state,
                FontCandidateState::Pending { .. },
            )));
            let selected = candidates[0];
            let font_id = state.materialize_font(selected).unwrap();
            // Loaded carries only the stable ID: no descriptor or point size
            // remains in the candidate. CoreText may keep its own references,
            // so native retain counts are not an application-owner assertion.
            assert!(matches!(
                state.font_candidates[selected].state,
                FontCandidateState::Loaded(id) if id == font_id,
            ));
            assert!(candidates[1..].iter().all(|&candidate| matches!(
                state.font_candidates[candidate].state,
                FontCandidateState::Pending { .. },
            )));
            let native = state.fonts[font_id.0].native_font();
            assert_eq!(state.materialize_font(selected).unwrap(), font_id);
            assert_eq!(
                state.fonts[font_id.0].native_font().as_concrete_TypeRef(),
                native.as_concrete_TypeRef(),
            );
            drop(state);

            let text = "office 中文 😀";
            let runs = [FontRun {
                font_id,
                len: text.len(),
            }];
            let before = system.layout_line(text, px(16.), &runs);
            system.release_idle_caches();
            let after = system.layout_line(text, px(16.), &runs);
            let mut state = system.0.write();
            assert_same_layout(&before, &after, &state, &state);
            assert_eq!(state.materialize_font(selected).unwrap(), font_id);
            assert_eq!(
                state.fonts[font_id.0].native_font().as_concrete_TypeRef(),
                native.as_concrete_TypeRef(),
            );
        });
    }

    pub fn failed_font_family_load_keeps_registered_state_unchanged() {
        objc::rc::autoreleasepool(|| {
            let system = MacTextSystem::new();
            let request = crate::font("Helvetica");
            let font_id = system.font_id(&request).unwrap();
            let text = "office 中文 😀";
            let runs = [FontRun {
                font_id,
                len: text.len(),
            }];
            let before = system.layout_line(text, px(16.), &runs);
            let family = SystemSource::new()
                .select_family_by_name("Helvetica")
                .unwrap();
            let invalid = Handle::from_memory(Arc::new(Vec::new()), 0);
            // Exercise font-kit's real parser error after one valid face. An
            // empty font file fails parsing; no loader or failure is mocked.
            assert!(invalid.load().is_err());
            let handles = [family.fonts()[0].clone(), invalid];
            {
                let mut state = system.0.write();
                let candidate_count = state.font_candidates.len();
                let font_count = state.fonts.len();
                let sources = state.font_sources_by_postscript_name.clone();
                let keys = state.font_candidates_by_font_key.clone();
                let selections = state.font_selections.clone();
                let names = state.postscript_names_by_font_id.clone();
                for _ in 0..3 {
                    assert!(
                        state
                            .load_family_handles(
                                &handles,
                                &FontFeatures::disable_ligatures(),
                                None,
                                false,
                            )
                            .is_err()
                    );
                    assert_eq!(state.font_candidates.len(), candidate_count);
                    assert_eq!(state.fonts.len(), font_count);
                    assert_eq!(state.font_sources_by_postscript_name, sources);
                    assert!(state.font_candidates_by_font_key == keys);
                    assert_eq!(state.font_selections, selections);
                    assert_eq!(state.postscript_names_by_font_id, names);
                }
            }
            assert_eq!(system.font_id(&request).unwrap(), font_id);
            let after = system.layout_line(text, px(16.), &runs);
            let state = system.0.read();
            assert_same_layout(&before, &after, &state, &state);
        });
    }

    pub fn native_text_scratch_retires_without_changing_fonts_or_layout() {
        objc::rc::autoreleasepool(|| {
            let fonts = MacTextSystem::new();
            assert_eq!(fonts.release_idle_caches(), 0);
            assert!(fonts.0.read().fonts.is_empty());

            let mut requested_font = crate::font("Helvetica").bold().italic();
            requested_font.features = FontFeatures::disable_ligatures();
            requested_font.fallbacks = Some(FontFallbacks::from_fonts(vec!["Menlo".into()]));
            let font_id = fonts.font_id(&requested_font).unwrap();
            let text = "office 中文 😀".repeat(32);
            let runs = text
                .chars()
                .map(|ch| FontRun {
                    font_id,
                    len: ch.len_utf8(),
                })
                .collect::<Vec<_>>();
            let before = fonts.layout_line(&text, px(16.), &runs);
            assert!(!before.runs.is_empty());
            assert!(before.runs.iter().any(|run| run.font_id != font_id));
            let (scratch_bytes, font_count, font_names) = {
                let state = fonts.0.read();
                (
                    state.zwnjs_scratch_space.capacity() * std::mem::size_of::<usize>(),
                    state.fonts.len(),
                    state.postscript_names_by_font_id.clone(),
                )
            };
            assert!(scratch_bytes > 0);
            assert_eq!(fonts.release_idle_caches(), scratch_bytes);
            assert_eq!(fonts.0.read().zwnjs_scratch_space.capacity(), 0);
            assert_eq!(fonts.release_idle_caches(), 0);

            // Keep the old layout alive across retirement. A later shape must
            // still resolve its base and fallback fonts to the same identities.
            assert_eq!(fonts.font_id(&requested_font).unwrap(), font_id);
            let after = fonts.layout_line(&text, px(16.), &runs);
            assert_eq!(before.font_size, after.font_size);
            assert_eq!(before.width, after.width);
            assert_eq!(before.ascent, after.ascent);
            assert_eq!(before.descent, after.descent);
            assert_eq!(before.len, after.len);
            assert_eq!(before.runs.len(), after.runs.len());
            for (before, after) in before.runs.iter().zip(&after.runs) {
                assert_eq!(before.font_id, after.font_id);
                assert_eq!(before.glyphs.len(), after.glyphs.len());
                for (before, after) in before.glyphs.iter().zip(&after.glyphs) {
                    assert_eq!(before.id, after.id);
                    assert_eq!(before.position, after.position);
                    assert_eq!(before.index, after.index);
                    assert_eq!(before.is_emoji, after.is_emoji);
                }
            }
            let state = fonts.0.read();
            assert_eq!(state.fonts.len(), font_count);
            assert_eq!(state.postscript_names_by_font_id, font_names);
            assert!(state.zwnjs_scratch_space.capacity() > 0);
            drop(state);
            assert!(fonts.release_idle_caches() > 0);
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{FontRun, GlyphId, MacTextSystem, PlatformTextSystem, font, px};

    #[test]
    fn test_layout_line_bom_char() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();
        let line = "\u{feff}";
        let mut style = FontRun {
            font_id,
            len: line.len(),
        };

        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert!(layout.runs.is_empty());

        let line = "a\u{feff}b";
        style.len = line.len();
        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b

        let line = "\u{feff}ab";
        let font_runs = &[
            FontRun {
                len: "\u{feff}".len(),
                font_id,
            },
            FontRun {
                len: "ab".len(),
                font_id,
            },
        ];
        let layout = fonts.layout_line(line, px(16.), font_runs);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b
    }

    #[test]
    fn test_layout_line_zwnj_insertion() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello world";
        let font_runs = &[
            FontRun { font_id, len: 5 }, // "hello"
            FontRun { font_id, len: 6 }, // " world"
        ];

        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with different font runs - should not insert ZWNJ
        let font_id2 = fonts.font_id(&font("Times")).unwrap_or(font_id);
        let font_runs_different = &[
            FontRun { font_id, len: 5 }, // "hello"
            // " world"
            FontRun {
                font_id: font_id2,
                len: 6,
            },
        ];

        let layout2 = fonts.layout_line(text, px(16.), font_runs_different);
        assert_eq!(layout2.len, text.len());

        for run in &layout2.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }
    }

    #[test]
    fn test_layout_line_zwnj_edge_cases() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello";
        let font_runs = &[FontRun { font_id, len: 5 }];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        let text = "abc";
        let font_runs = &[
            FontRun { font_id, len: 1 }, // "a"
            FontRun { font_id, len: 1 }, // "b"
            FontRun { font_id, len: 1 }, // "c"
        ];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with empty text
        let text = "";
        let font_runs = &[];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, 0);
        assert!(layout.runs.is_empty());
    }
}
