//! Own dynamically generated file icons without GPUI's process-wide asset cache.
//!
//! Both indexes are bounded. PNG content, rather than its filesystem path, owns
//! the decoded image so aliases cannot evict one another's atlas entry. The
//! shared idle task also retires the hidden native window, including its atlas.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastab_gpui::{OverlayHandle, OverlayState};
use gpui::{App, Context, Entity, Image, ImageId, RenderImage, Task, WeakEntity};

const CAPACITY: usize = 64;
const MAX_UNCACHED_PER_RESULT: usize = 8;
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const ICON_PIXELS: u32 = 64;
const MAX_SOURCE_SIDE: u32 = 1024;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

struct CachedIcon {
    image: Arc<RenderImage>,
    last_used: u64,
    cached: bool,
}

struct PreparedIcon {
    content: u64,
    image: Arc<RenderImage>,
}

struct IconBatch {
    paths: Vec<PathBuf>,
    cwd: String,
    state: WeakEntity<OverlayState>,
    slot: Arc<Mutex<Option<OverlayHandle>>>,
    generation: Arc<AtomicU64>,
    expected_generation: u64,
    batch_generation: u64,
    revision: (u64, u64),
    range: Range<usize>,
}

#[derive(Default)]
pub(super) struct FileIconCache {
    paths: HashMap<PathBuf, (u64, u64)>,
    images: HashMap<u64, CachedIcon>,
    batch: HashSet<u64>,
    clock: u64,
    idle_generation: u64,
    idle_task: Option<Task<()>>,
    requested: Vec<PathBuf>,
    pending: Option<IconBatch>,
    load_task: Option<Task<()>>,
    batch_generation: u64,
    viewport: Option<((u64, u64), Range<usize>)>,
    #[cfg(test)]
    provider: Option<Arc<dyn Fn(&Path) -> Option<Vec<u8>> + Send + Sync>>,
}

impl FileIconCache {
    pub(super) fn diagnostics(&self) -> fastab_engine::HostResourceDiagnostics {
        fastab_engine::HostResourceDiagnostics {
            file_icon_paths: self.paths.len(),
            file_icon_images: self.images.len(),
            file_icon_worker_active: self.load_task.is_some(),
            file_icon_pending_paths: self.pending.as_ref().map_or(0, |batch| batch.paths.len()),
            ..Default::default()
        }
    }

    pub(super) fn begin_batch(&mut self) {
        self.cancel_idle();
        self.batch.clear();
        self.requested.clear();
        self.pending = None;
        self.viewport = None;
        self.batch_generation = self.batch_generation.wrapping_add(1);
    }

    pub(super) fn prepare_visible(
        &mut self,
        cwd: &str,
        state: &Entity<OverlayState>,
        revision: (u64, u64),
        range: Range<usize>,
        cx: &mut App,
    ) -> bool {
        if !state.read(cx).visible || state.read(cx).icon_request_revision() != revision {
            return false;
        }
        let range = range.start.min(state.read(cx).items.len())..range.end.min(state.read(cx).items.len());
        if self.viewport.as_ref() == Some(&(revision, range.clone())) {
            return false;
        }
        self.begin_batch();
        self.viewport = Some((revision, range.clone()));
        let mut uncached = 0;
        state.update(cx, |state, cx| {
            let mut changed = false;
            for (index, item) in state.items.iter_mut().enumerate() {
                let image = if range.contains(&index) && needs_native_icon(item) {
                    self.lookup(cwd, &item.name, &item.kind, &mut uncached)
                } else {
                    None
                };
                if item.icon_png.as_ref().map(|image| image.id) != image.as_ref().map(|image| image.id) {
                    item.icon_png = image;
                    changed = true;
                }
            }
            if changed {
                cx.notify();
            }
        });
        true
    }

    pub(super) fn cancel_idle(&mut self) {
        self.idle_generation = self.idle_generation.wrapping_add(1);
        self.idle_task.take();
    }

    pub(super) fn lookup(
        &mut self,
        cwd: &str,
        name: &str,
        kind: &str,
        uncached: &mut usize,
    ) -> Option<Arc<RenderImage>> {
        if kind != "file" && kind != "folder" {
            return None;
        }
        let path = icon_path(cwd, name);
        if let Some(&(content, _)) = self.paths.get(&path) {
            return self.touch(path, content);
        }
        if *uncached >= MAX_UNCACHED_PER_RESULT {
            return None;
        }
        *uncached += 1;
        // No filesystem or AppKit work on the foreground executor. The first
        // frame uses the bundled fallback while one bounded worker loads icons.
        if !self.requested.contains(&path) {
            self.requested.push(path);
        }
        None
    }

    #[cfg(test)]
    fn insert_png(&mut self, path: PathBuf, bytes: Vec<u8>) -> Option<Arc<RenderImage>> {
        self.insert_prepared(path, prepare_icon(bytes)?)
    }

    fn insert_prepared(&mut self, path: PathBuf, prepared: PreparedIcon) -> Option<Arc<RenderImage>> {
        let content = prepared.content;
        if !self.images.get(&content).is_some_and(|entry| entry.cached) {
            self.reserve_content()?;
            if let Some(entry) = self.images.get_mut(&content) {
                entry.cached = true;
            } else {
                self.images.insert(
                    content,
                    CachedIcon {
                        image: prepared.image,
                        last_used: 0,
                        cached: true,
                    },
                );
            }
        }
        self.touch(path, content)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn load_requested(
        &mut self,
        cwd: &str,
        state: WeakEntity<OverlayState>,
        slot: Arc<Mutex<Option<OverlayHandle>>>,
        generation: Arc<AtomicU64>,
        expected_generation: u64,
        cx: &mut Context<'_, Self>,
    ) {
        if self.requested.is_empty() {
            return;
        }
        let Some(overlay) = state.upgrade() else { return };
        let revision = overlay.read(cx).icon_request_revision();
        let range = self
            .viewport
            .as_ref()
            .map_or(0..overlay.read(cx).items.len(), |(_, range)| range.clone());
        self.pending = Some(IconBatch {
            paths: std::mem::take(&mut self.requested),
            cwd: cwd.to_owned(),
            state,
            slot,
            generation,
            expected_generation,
            batch_generation: self.batch_generation,
            revision,
            range,
        });
        self.start_pending(cx);
    }

    fn start_pending(&mut self, cx: &mut Context<'_, Self>) {
        // Keep the real worker's slot occupied until it returns, including a
        // synchronous native call that cannot be cancelled. New results replace
        // just one pending batch; hiding never spawns replacement workers.
        if self.load_task.is_some() {
            return;
        }
        let Some(mut batch) = self.pending.take() else { return };
        let Some(state) = batch.state.upgrade() else { return };
        if !state.read(cx).visible || batch.generation.load(Ordering::Relaxed) != batch.expected_generation {
            return;
        }
        let paths = std::mem::take(&mut batch.paths);
        let generation = batch.generation.clone();
        let expected = batch.expected_generation;
        let (sender, receiver) = futures::channel::oneshot::channel();
        #[cfg(test)]
        let provider = self.provider.clone();
        let spawned = std::thread::Builder::new()
            .name("fastab-file-icons".into())
            .spawn(move || {
                #[cfg(test)]
                let provider = |path: &Path| provider.as_ref().map_or_else(|| native_png(path), |load| load(path));
                #[cfg(not(test))]
                let provider = native_png;
                let icons = load_icons(paths, &generation, expected, provider);
                let _ = sender.send(icons);
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "Could not start file icon worker");
            return;
        }
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let _ = this.update(cx, |cache, cx| {
                if let Some(task) = cache.load_task.take() {
                    task.detach();
                }
                if let Ok(icons) = result {
                    cache.apply_loaded(&batch, icons, cx);
                }
                cache.start_pending(cx);
            });
        }));
    }

    fn apply_loaded(&mut self, batch: &IconBatch, icons: Vec<(PathBuf, PreparedIcon)>, cx: &mut App) {
        if self.batch_generation != batch.batch_generation
            || batch.generation.load(Ordering::Relaxed) != batch.expected_generation
        {
            return;
        }
        let Some(state) = batch.state.upgrade() else { return };
        if !state.read(cx).visible || state.read(cx).icon_request_revision() != batch.revision {
            return;
        }
        let used = row_images(&state, cx);
        self.batch = self
            .images
            .iter()
            .filter(|(_, icon)| used.contains(&icon.image.id))
            .map(|(&id, _)| id)
            .collect();
        let images = icons
            .into_iter()
            .filter_map(|(path, icon)| self.insert_prepared(path.clone(), icon).map(|image| (path, image)))
            .collect::<HashMap<_, _>>();
        state.update(cx, |state, cx| {
            for (index, item) in state.items.iter_mut().enumerate() {
                if batch.range.contains(&index) && needs_native_icon(item) {
                    let path = icon_path(&batch.cwd, &item.name);
                    if let Some(image) = images.get(&path) {
                        item.icon_png = Some(image.clone());
                    }
                }
            }
            cx.notify();
        });
        self.finish_batch(&state, &batch.slot, cx);
    }

    fn reserve_content(&mut self) -> Option<()> {
        if self.images.values().filter(|entry| entry.cached).count() < CAPACITY {
            return Some(());
        }
        // A large result must not evict icons already assigned to its earlier
        // rows. Additional unique icons use the normal bundled fallback.
        let victim = self
            .images
            .iter()
            .filter(|(content, entry)| entry.cached && !self.batch.contains(content))
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(&content, _)| content)?;
        self.images.get_mut(&victim)?.cached = false;
        self.paths.retain(|_, (content, _)| *content != victim);
        Some(())
    }

    fn touch(&mut self, path: PathBuf, content: u64) -> Option<Arc<RenderImage>> {
        self.clock = self.clock.saturating_add(1);
        let entry = self.images.get_mut(&content)?;
        entry.last_used = self.clock;
        self.batch.insert(content);
        let image = entry.image.clone();
        if !self.paths.contains_key(&path) && self.paths.len() >= CAPACITY {
            if let Some(old) = self
                .paths
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(path, _)| path.clone())
            {
                self.paths.remove(&old);
            }
        }
        self.paths.insert(path, (content, self.clock));
        Some(image)
    }

    fn take_retired(&mut self, used: &HashSet<ImageId>) -> Vec<Arc<RenderImage>> {
        let retired = self
            .images
            .iter()
            .filter(|(_, entry)| !entry.cached && !used.contains(&entry.image.id))
            .map(|(&content, _)| content)
            .collect::<Vec<_>>();
        retired
            .into_iter()
            .filter_map(|content| self.images.remove(&content).map(|entry| entry.image))
            .collect()
    }

    pub(super) fn finish_batch(
        &mut self,
        state: &Entity<OverlayState>,
        slot: &Mutex<Option<OverlayHandle>>,
        cx: &mut App,
    ) {
        let used = row_images(state, cx);
        let retired = self.take_retired(&used);
        release_atlas_images(retired, slot, cx);
        self.batch.clear();
    }

    pub(super) fn schedule_idle(
        &mut self,
        state: WeakEntity<OverlayState>,
        slot: Arc<Mutex<Option<OverlayHandle>>>,
        cx: &mut Context<'_, Self>,
    ) {
        // Even an overlay with only text and bundled icons owns a renderer.
        // Repeated hidden input/focus events must not postpone its retirement.
        // Showing or beginning a new result batch cancels this grace period.
        if self.idle_task.is_some() {
            return;
        }
        self.cancel_idle();
        let generation = self.idle_generation;
        self.idle_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(IDLE_TIMEOUT).await;
            let _ = this.update(cx, |cache, cx| {
                if cache.idle_generation != generation {
                    return;
                }
                // Release the completed task's slot even if state disappeared
                // or became visible, so the next hide can start a new period.
                if let Some(task) = cache.idle_task.take() {
                    task.detach();
                }
                let Some(state) = state.upgrade() else { return };
                if state.read(cx).visible {
                    return;
                }
                let handle = *slot.lock().unwrap_or_else(|err| err.into_inner());
                if let Some(handle) = handle {
                    // Invalidate queued AppKit positioning before dropping the
                    // native window those requests address. Window destruction
                    // releases its scenes, renderer and atlas together.
                    let _ = fastab_gpui::park_overlay_handle(&handle, cx);
                    let _ = gpui::AnyWindowHandle::from(handle).update(cx, |_, window, _| {
                        window.remove_window();
                    });
                    super::clear_overlay_handle_if(&slot, handle);
                }
                cache.batch_generation = cache.batch_generation.wrapping_add(1);
                cache.pending = None;
                cache.viewport = None;
                cache.requested.clear();
                cache.paths.clear();
                for entry in cache.images.values_mut() {
                    entry.cached = false;
                }
                // Explicit Show/Tab now requests fresh rows. Preserve only
                // selection identity, not hidden result strings or images.
                state.update(cx, |state, _| state.release_hidden_payload());
                drop(cache.take_retired(&row_images(&state, cx)));
                cache.batch.clear();
            });
        }));
    }
}

fn needs_native_icon(item: &fastab_gpui::SuggestionItem) -> bool {
    (item.kind == "file" || item.kind == "folder")
        && !item
            .icon_identifier
            .as_deref()
            .is_some_and(fastab_gpui::identifier_is_renderable)
}

fn icon_path(cwd: &str, name: &str) -> PathBuf {
    PathBuf::from(if cwd.is_empty() { "." } else { cwd }).join(name.trim_end_matches('/'))
}

fn native_png(path: &Path) -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        // Each synchronous leaf drains its own autorelease pool; only owned
        // bytes leave AppKit, never an NSImage or graphics context.
        unsafe { macos_utils::image::png_for_path(path) }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

fn load_icons(
    paths: Vec<PathBuf>,
    generation: &AtomicU64,
    expected: u64,
    provider: impl Fn(&Path) -> Option<Vec<u8>>,
) -> Vec<(PathBuf, PreparedIcon)> {
    let mut icons = Vec::new();
    for path in paths {
        if generation.load(Ordering::Relaxed) != expected {
            break;
        }
        if let Some(icon) = provider(&path).and_then(prepare_icon) {
            icons.push((path, icon));
        }
    }
    icons
}

fn prepare_icon(bytes: Vec<u8>) -> Option<PreparedIcon> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let source = Image::from_bytes(gpui::ImageFormat::Png, bytes);
    Some(PreparedIcon {
        content: source.id(),
        image: decode_icon(&source.bytes)?,
    })
}

fn decode_icon(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    let source = reader.decode().ok()?;
    let source = if source.width() > ICON_PIXELS || source.height() > ICON_PIXELS {
        source.thumbnail(ICON_PIXELS, ICON_PIXELS)
    } else {
        source
    };
    let mut pixels = source.into_rgba8();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new([image::Frame::new(pixels)])))
}

fn row_images(state: &Entity<OverlayState>, cx: &App) -> HashSet<ImageId> {
    state
        .read(cx)
        .items
        .iter()
        .filter_map(|item| item.icon_png.as_ref().map(|image| image.id))
        .collect()
}

fn release_atlas_images(images: Vec<Arc<RenderImage>>, slot: &Mutex<Option<OverlayHandle>>, cx: &mut App) {
    if images.is_empty() {
        return;
    }
    let handle = *slot.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(handle) = handle {
        // A typed WindowHandle::update would lease SuggestionList while draw
        // needs to update that same root entity. Only borrow the window here.
        let _ = gpui::AnyWindowHandle::from(handle).update(cx, |_, window, cx| {
            // This runs from desktop events, after the rows'
            // entity update has returned, never from a render/paint callback.
            // Rebuild the CPU scene before removing tiles it previously used.
            // draw does not present; the hidden root is an empty div. Already
            // committed Metal commands retain their own texture references.
            window.refresh();
            window.draw(cx).clear();
            for image in &images {
                let _ = window.drop_image(image.clone());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    fn png(value: u8) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(32, 32, image::Rgba([value, 31, 63, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[gpui::test]
    fn blocked_worker_keeps_one_slot_and_latest_pending_batch(cx: &mut gpui::TestAppContext) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        let generation = Arc::new(AtomicU64::new(1));
        let slot = Arc::new(Mutex::new(None));
        let (cache, state) = cx.update(|cx| {
            let state = cx.new(|_| OverlayState::new());
            state.update(cx, |state, _| state.visible = true);
            let cache = cx.new(|_| FileIconCache {
                provider: Some(Arc::new(move |path| {
                    started_tx.send(path.to_owned()).unwrap();
                    release.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
                    Some(png(1))
                })),
                ..Default::default()
            });
            cache.update(cx, |icons, cx| {
                icons.begin_batch();
                icons.lookup("/tmp", "first", "file", &mut 0);
                icons.load_requested("/tmp", state.downgrade(), slot.clone(), generation.clone(), 1, cx);
            });
            (cache, state)
        });
        cx.run_until_parked();
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            PathBuf::from("/tmp/first")
        );
        for (next, name) in [(2, "superseded"), (3, "latest")] {
            generation.store(next, Ordering::Relaxed);
            cx.update(|cx| {
                cache.update(cx, |icons, cx| {
                    icons.begin_batch();
                    icons.lookup("/tmp", name, "file", &mut 0);
                    icons.load_requested("/tmp", state.downgrade(), slot.clone(), generation.clone(), next, cx);
                })
            });
        }
        assert!(
            started_rx.try_recv().is_err(),
            "blocked native call retains the only worker slot"
        );
        release_tx.send(()).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            cx.run_until_parked();
            if let Ok(path) = started_rx.try_recv() {
                assert_eq!(path, PathBuf::from("/tmp/latest"));
                break;
            }
            assert!(std::time::Instant::now() < until, "latest pending worker did not start");
            std::thread::yield_now();
        }
        cx.update(|cx| {
            state.update(cx, |state, _| state.visible = false);
            cache.update(cx, |icons, cx| icons.schedule_idle(state.downgrade(), slot.clone(), cx));
        });
        cx.run_until_parked();
        cx.executor().advance_clock(IDLE_TIMEOUT);
        cx.run_until_parked();
        release_tx.send(()).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            cx.run_until_parked();
            if cx.update(|cx| cache.read(cx).load_task.is_none()) {
                break;
            }
            assert!(std::time::Instant::now() < until, "native worker did not settle");
            std::thread::yield_now();
        }
        cx.update(|cx| {
            assert!(cache.read(cx).paths.is_empty());
            assert!(cache.read(cx).images.is_empty());
            assert!(slot.lock().unwrap().is_none());
        });
    }

    #[test]
    fn stale_native_work_stops_before_the_next_path() {
        let generation = AtomicU64::new(1);
        let calls = std::cell::Cell::new(0);
        let result = load_icons(
            vec![PathBuf::from("first"), PathBuf::from("second")],
            &generation,
            1,
            |_| {
                calls.set(calls.get() + 1);
                generation.store(2, Ordering::Relaxed);
                Some(png(1))
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(result.len(), 1);
    }

    #[gpui::test]
    fn only_visible_rows_without_renderable_icons_request_native_work(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let state = cx.new(|_| OverlayState::new());
            state.update(cx, |state, _| {
                state.items = [
                    "",
                    "fig://icon?type=file",
                    "📁",
                    "fig://template?badge=R",
                    "fig://icon?type=not-real",
                    "",
                ]
                .into_iter()
                .enumerate()
                .map(|(index, icon)| fastab_gpui::SuggestionItem {
                    name: index.to_string(),
                    kind: "file".into(),
                    icon_identifier: Some(icon.into()),
                    ..Default::default()
                })
                .collect();
                state.visible = true;
            });
            let mut cache = FileIconCache::default();
            let revision = state.read(cx).icon_request_revision();
            assert!(cache.prepare_visible("/tmp", &state, revision, 1..5, cx));
            assert_eq!(cache.requested, vec![PathBuf::from("/tmp/4")]);
            assert!(!cache.prepare_visible("/tmp", &state, revision, 1..5, cx));
            let png = cache.insert_png(PathBuf::from("/tmp/4"), png(1)).unwrap();
            state.update(cx, |state, _| state.items[4].icon_png = Some(png));
            assert!(cache.prepare_visible("/tmp", &state, revision, 5..6, cx));
            assert_eq!(cache.requested, vec![PathBuf::from("/tmp/5")]);
            assert!(
                state.read(cx).items[4].icon_png.is_none(),
                "offscreen rows release their image"
            );
            assert!(!cache.prepare_visible("/tmp", &state, (revision.0 + 1, revision.1), 0..1, cx));
            state.update(cx, |state, _| state.visible = false);
            assert!(!cache.prepare_visible("/tmp", &state, revision, 0..1, cx));
        });
    }

    #[gpui::test]
    fn late_icons_do_not_refill_hidden_or_replaced_results(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let state = cx.new(|_| OverlayState::new());
            state.update(cx, |state, _| {
                state.visible = true;
                state.items.push(fastab_gpui::SuggestionItem {
                    name: "file".into(),
                    kind: "file".into(),
                    ..Default::default()
                });
            });
            let mut cache = FileIconCache::default();
            cache.begin_batch();
            let generation = Arc::new(AtomicU64::new(1));
            let batch = IconBatch {
                paths: Vec::new(),
                cwd: "/tmp".into(),
                state: state.downgrade(),
                slot: Arc::new(Mutex::new(None)),
                generation: generation.clone(),
                expected_generation: 1,
                batch_generation: cache.batch_generation,
                revision: state.read(cx).icon_request_revision(),
                range: 0..1,
            };
            let result = || vec![(PathBuf::from("/tmp/file"), prepare_icon(png(1)).unwrap())];
            generation.store(2, Ordering::Relaxed);
            cache.apply_loaded(&batch, result(), cx);
            assert!(cache.images.is_empty());
            generation.store(1, Ordering::Relaxed);
            state.update(cx, |state, _| state.visible = false);
            cache.apply_loaded(&batch, result(), cx);
            assert!(cache.images.is_empty());
            state.update(cx, |state, _| state.visible = true);
            cache.apply_loaded(&batch, result(), cx);
            assert!(state.read(cx).items[0].icon_png.is_some());
            cache.begin_batch();
            let before = cache.images.len();
            cache.apply_loaded(&batch, result(), cx);
            assert_eq!(cache.images.len(), before);
        });
    }

    #[gpui::test]
    fn idle_grace_restarts_and_releases_hidden_rows_and_images(cx: &mut gpui::TestAppContext) {
        let slot = Arc::new(Mutex::new(None));
        let (cache, state, unused, kept) = cx.update(|cx| {
            let state = cx.new(|_| OverlayState::new());
            let cache = cx.new(|_| FileIconCache::default());
            let (unused, kept) = cache.update(cx, |cache, cx| {
                let unused = cache.insert_png(PathBuf::from("unused"), png(1)).unwrap();
                let kept = cache.insert_png(PathBuf::from("kept"), png(2)).unwrap();
                state.update(cx, |state, _| {
                    // A large hidden result survives the grace, then releases
                    // rows and backing storage before a cold Show/Tab request.
                    state.items.reserve(10_000);
                    state.items.push(fastab_gpui::SuggestionItem {
                        name: "kept".into(),
                        icon_png: Some(kept.clone()),
                        ..Default::default()
                    });
                    state.search_term = "kept search".into();
                    state.match_term = "kept match".into();
                    state.set_current_arg("kept argument", "kept argument description");
                });
                cache.schedule_idle(state.downgrade(), slot.clone(), cx);
                (Arc::downgrade(&unused), Arc::downgrade(&kept))
            });
            (cache, state, unused, kept)
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(6));
        // Showing cancels the old idle period. Hiding again starts a new one.
        cx.update(|cx| cache.update(cx, |cache, _| cache.cancel_idle()));
        cx.executor().advance_clock(Duration::from_secs(6));
        cx.run_until_parked();
        assert!(unused.upgrade().is_some());
        cx.update(|cx| {
            cache.update(cx, |cache, cx| {
                cache.schedule_idle(state.downgrade(), slot.clone(), cx);
            });
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(9));
        cx.run_until_parked();
        assert!(unused.upgrade().is_some(), "a new hide gets the full ten seconds");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(unused.upgrade().is_none());
        assert!(kept.upgrade().is_none(), "hidden rows no longer pin decoded images");
        cx.update(|cx| {
            assert!(cache.read(cx).paths.is_empty());
            assert!(cache.read(cx).idle_task.is_none());
            state.update(cx, |state, _| {
                assert!(state.items.is_empty());
                assert_eq!(state.items.capacity(), 0);
                assert!(state.search_term.is_empty());
                assert!(state.match_term.is_empty());
                assert!(state.current_arg_name.is_empty());
                assert!(state.current_arg_description.is_empty());
                state.dismiss();
            });
            cache.update(cx, |cache, cx| cache.finish_batch(&state, &slot, cx));
        });
        assert!(kept.upgrade().is_none());

        // Completing one grace period must not block a later independent one.
        let next = cx.update(|cx| {
            cache.update(cx, |cache, cx| {
                let image = cache.insert_png(PathBuf::from("next"), png(3)).unwrap();
                cache.schedule_idle(state.downgrade(), slot.clone(), cx);
                Arc::downgrade(&image)
            })
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(9));
        cx.run_until_parked();
        assert!(next.upgrade().is_some());
        cx.update(|cx| {
            let state = state.read(cx);
            assert!(state.items.is_empty());
            assert_eq!(state.items.capacity(), 0);
            assert_eq!(state.search_term.capacity(), 0);
            assert_eq!(state.match_term.capacity(), 0);
            assert_eq!(state.current_arg_name.capacity(), 0);
            assert_eq!(state.current_arg_description.capacity(), 0);
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(next.upgrade().is_none());
        cx.update(|cx| {
            let state = state.read(cx);
            assert_eq!(state.items.capacity(), 0);
            assert_eq!(state.search_term.capacity(), 0);
            assert_eq!(state.match_term.capacity(), 0);
            assert_eq!(state.current_arg_name.capacity(), 0);
            assert_eq!(state.current_arg_description.capacity(), 0);
        });
    }

    #[gpui::test]
    fn repeated_hides_keep_the_original_idle_deadline(cx: &mut gpui::TestAppContext) {
        let slot = Arc::new(Mutex::new(None));
        let (cache, state, image) = cx.update(|cx| {
            let state = cx.new(|_| OverlayState::new());
            let cache = cx.new(|_| FileIconCache::default());
            let image = cache.update(cx, |cache, cx| {
                let image = cache.insert_png(PathBuf::from("unused"), png(1)).unwrap();
                cache.schedule_idle(state.downgrade(), slot.clone(), cx);
                Arc::downgrade(&image)
            });
            (cache, state, image)
        });
        cx.run_until_parked();
        for elapsed in [6, 3] {
            cx.executor().advance_clock(Duration::from_secs(elapsed));
            cx.run_until_parked();
            assert!(image.upgrade().is_some());
            cx.update(|cx| {
                cache.update(cx, |cache, cx| {
                    cache.schedule_idle(state.downgrade(), slot.clone(), cx);
                });
            });
            cx.run_until_parked();
        }
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            image.upgrade().is_none(),
            "repeated hides still retire at the first deadline"
        );
    }

    #[gpui::test]
    fn an_idle_task_that_skips_retirement_can_be_scheduled_again(cx: &mut gpui::TestAppContext) {
        for keep_visible_state in [false, true] {
            let slot = Arc::new(Mutex::new(None));
            let (cache, state, image) = cx.update(|cx| {
                let state = cx.new(|_| OverlayState::new());
                let cache = cx.new(|_| FileIconCache::default());
                let image = cache.update(cx, |cache, cx| {
                    let image = cache.insert_png(PathBuf::from("unused"), png(1)).unwrap();
                    cache.schedule_idle(state.downgrade(), slot.clone(), cx);
                    Arc::downgrade(&image)
                });
                state.update(cx, |state, _| state.visible = true);
                (cache, keep_visible_state.then_some(state), image)
            });
            cx.run_until_parked();
            cx.executor().advance_clock(IDLE_TIMEOUT);
            cx.run_until_parked();
            assert!(image.upgrade().is_some(), "missing or visible state skips retirement");
            let state = cx.update(|cx| {
                assert!(cache.read(cx).idle_task.is_none());
                let state = state.unwrap_or_else(|| cx.new(|_| OverlayState::new()));
                state.update(cx, |state, _| state.visible = false);
                cache.update(cx, |cache, cx| {
                    cache.schedule_idle(state.downgrade(), slot.clone(), cx);
                });
                state
            });
            cx.run_until_parked();
            cx.executor().advance_clock(IDLE_TIMEOUT);
            cx.run_until_parked();
            assert!(image.upgrade().is_none());
            drop(state);
        }
    }

    #[test]
    fn unique_pngs_retire_decoded_images_instead_of_accumulating() {
        let mut cache = FileIconCache::default();
        let mut weak = Vec::new();
        for index in 0..96u8 {
            cache.begin_batch();
            let image = cache.insert_png(PathBuf::from(index.to_string()), png(index)).unwrap();
            weak.push(Arc::downgrade(&image));
            drop(image);
            drop(cache.take_retired(&HashSet::new()));
            assert!(cache.images.len() <= CAPACITY);
            assert!(cache.paths.len() <= CAPACITY);
        }
        assert_eq!(weak.iter().filter(|image| image.upgrade().is_some()).count(), CAPACITY);
        drop(cache);
        assert!(weak.iter().all(|image| image.upgrade().is_none()));
    }

    #[test]
    fn aliases_share_content_and_current_batch_cannot_evict_itself() {
        let mut cache = FileIconCache::default();
        cache.begin_batch();
        let first = cache.insert_png(PathBuf::from("a"), png(0)).unwrap();
        for index in 0..96u8 {
            let alias = cache
                .insert_png(PathBuf::from(format!("alias-{index}")), png(0))
                .unwrap();
            assert!(Arc::ptr_eq(&first, &alias));
        }
        assert_eq!(cache.images.len(), 1);
        assert_eq!(cache.paths.len(), CAPACITY);
        for index in 1..CAPACITY as u8 {
            assert!(cache.insert_png(PathBuf::from(index.to_string()), png(index)).is_some());
        }
        assert!(cache.insert_png(PathBuf::from("overflow"), png(255)).is_none());
        assert!(cache.take_retired(&HashSet::new()).is_empty());
        assert_eq!(cache.images.len(), CAPACITY);
    }

    #[test]
    fn rows_pin_retired_images_until_their_scene_can_be_replaced() {
        let mut cache = FileIconCache::default();
        let image = cache.insert_png(PathBuf::from("visible"), png(0)).unwrap();
        let weak = Arc::downgrade(&image);
        let used = HashSet::from([image.id]);
        cache.paths.clear();
        cache.images.values_mut().for_each(|entry| entry.cached = false);
        drop(image);
        assert!(cache.take_retired(&used).is_empty());
        assert!(weak.upgrade().is_some());
        let retiring = cache.take_retired(&HashSet::new());
        assert!(
            weak.upgrade().is_some(),
            "the retirement batch owns the image until atlas cleanup"
        );
        drop(retiring);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn large_png_is_reduced_to_icon_pixels_and_oversize_input_is_rejected() {
        let mut cache = FileIconCache::default();
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(768, 512, image::Rgba([7, 31, 63, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let image = cache.insert_png(PathBuf::from("large"), bytes.into_inner()).unwrap();
        let size = image.size(0);
        assert_eq!(size.width.0, ICON_PIXELS as i32);
        assert!(size.height.0 > 0 && size.height.0 <= ICON_PIXELS as i32);
        assert_eq!(&image.as_bytes(0).unwrap()[..4], &[63, 31, 7, 255]);
        assert!(image.as_bytes(0).unwrap().len() <= (ICON_PIXELS * ICON_PIXELS * 4) as usize);

        let mut oversized = Cursor::new(Vec::new());
        image::RgbaImage::new(MAX_SOURCE_SIDE + 1, 1)
            .write_to(&mut oversized, image::ImageFormat::Png)
            .unwrap();
        assert!(
            cache
                .insert_png(PathBuf::from("oversized"), oversized.into_inner())
                .is_none()
        );
        assert!(
            cache
                .insert_png(PathBuf::from("too-many-bytes"), vec![0; MAX_SOURCE_BYTES + 1])
                .is_none()
        );
    }
}
