use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};

use gpui::{App, AppContext as _, Asset, Empty, LineFragment, TestAppContext, TextRun, font, px};
use parking_lot::Mutex;

#[gpui::test]
fn borrowed_wrapper_survives_release_without_repopulating_retired_pool(cx: &mut TestAppContext) {
    let text = cx.update(|cx| cx.text_system().clone());
    let descriptor = font("Helvetica");
    let fragments = [LineFragment::Text {
        text: "hello 世界 from Fastab",
    }];
    let mut borrowed = text.line_wrapper(descriptor.clone(), px(16.));
    let expected = borrowed.wrap_line(&fragments, px(60.)).collect::<Vec<_>>();
    assert!(!expected.is_empty());

    let released = cx.update(|cx| cx.release_idle_caches().unwrap());
    assert_eq!(released.text.line_wrappers, 0);
    assert_eq!(borrowed.wrap_line(&fragments, px(60.)).collect::<Vec<_>>(), expected);

    // A replacement can already occupy the same key when the old lease returns.
    let mut replacement = text.line_wrapper(descriptor.clone(), px(16.));
    assert_eq!(replacement.wrap_line(&fragments, px(60.)).collect::<Vec<_>>(), expected);
    drop(replacement);
    drop(borrowed);
    let released = cx.update(|cx| cx.release_idle_caches().unwrap());
    assert_eq!(released.text.line_wrappers, 1);

    let mut reopened = text.line_wrapper(descriptor, px(16.));
    assert_eq!(reopened.wrap_line(&fragments, px(60.)).collect::<Vec<_>>(), expected);
    drop(reopened);
    assert_eq!(cx.update(|cx| cx.release_idle_caches().unwrap()).text.line_wrappers, 1);
}

#[gpui::test]
fn font_identity_and_existing_layout_survive_cache_release(cx: &mut TestAppContext) {
    let descriptor = font("Helvetica");
    let text = cx.update(|cx| cx.text_system().clone());
    let font_id = text.resolve_font(&descriptor);
    let original_bounds = text.bounding_box(font_id, px(16.));
    let value = "hello 世界 👋";
    let runs = [TextRun {
        len: value.len(),
        font: descriptor.clone(),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }];
    let window = cx.add_window(|_, _| Empty);
    let window_text = window.update(cx, |_, window, _| window.text_system().clone()).unwrap();
    let original_layout = window_text.layout_line(value, px(16.), &runs, None);
    assert_eq!(original_layout.len, value.len());
    window.update(cx, |_, window, _| window.remove_window()).unwrap();

    let released = cx.update(|cx| cx.release_idle_caches().unwrap());
    assert_eq!(released.text.font_metrics, 1);
    assert_eq!(released.text.font_run_buffers, 1);
    assert!(released.text.font_run_capacity > 0);
    assert_eq!(text.get_font_for_id(font_id), Some(descriptor.clone()));
    assert_eq!(text.resolve_font(&descriptor), font_id);
    assert_eq!(text.bounding_box(font_id, px(16.)), original_bounds);

    // The window's text system and layouts may be retained by external consumers.
    let layout = window_text.layout_line(value, px(16.), &runs, None);
    assert_eq!(layout.width, original_layout.width);
    assert_eq!(layout.runs.len(), original_layout.runs.len());
    assert_eq!(
        cx.update(|cx| cx.release_idle_caches().unwrap()).text.font_run_buffers,
        1
    );
    assert_eq!(
        cx.update(|cx| cx.release_idle_caches().unwrap()).text.font_run_buffers,
        0
    );
}

#[derive(Default)]
struct AssetGate {
    ready: bool,
    waker: Option<Waker>,
}

#[derive(Clone, Default)]
struct GatedSource(Arc<Mutex<Vec<SharedGate>>>);

type SharedGate = Arc<Mutex<AssetGate>>;

impl Hash for GatedSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}

impl GatedSource {
    fn finish(&self, index: usize) {
        let gate = self.0.lock()[index].clone();
        let waker = {
            let mut gate = gate.lock();
            gate.ready = true;
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct GatedAsset;

impl Asset for GatedAsset {
    type Source = GatedSource;
    type Output = Arc<usize>;

    fn load(source: Self::Source, _cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let gate = Arc::new(Mutex::new(AssetGate::default()));
        let index = {
            let mut loads = source.0.lock();
            let index = loads.len();
            loads.push(gate.clone());
            index
        };
        poll_fn(move |cx| {
            let mut gate = gate.lock();
            if gate.ready {
                Poll::Ready(Arc::new(index))
            } else {
                gate.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}

#[gpui::test]
async fn late_asset_completion_does_not_replace_a_new_load(cx: &mut TestAppContext) {
    let source = GatedSource::default();
    let (old_task, first) = cx.update(|cx| cx.fetch_asset::<GatedAsset>(&source));
    assert!(first);
    assert_eq!(cx.update(|cx| cx.release_idle_caches().unwrap()).assets, 1);
    let (new_task, first) = cx.update(|cx| cx.fetch_asset::<GatedAsset>(&source));
    assert!(first);

    source.finish(0);
    assert_eq!(*old_task.await, 0);
    let (cached_task, first) = cx.update(|cx| cx.fetch_asset::<GatedAsset>(&source));
    assert!(!first);
    assert_eq!(source.0.lock().len(), 2);
    source.finish(1);
    let result = new_task.await;
    let cached_result = cached_task.await;
    assert_eq!(*result, 1);
    assert!(Arc::ptr_eq(&result, &cached_result));

    let weak = Arc::downgrade(&result);
    drop(result);
    drop(cached_result);
    assert!(weak.upgrade().is_some());
    assert_eq!(cx.update(|cx| cx.release_idle_caches().unwrap()).assets, 1);
    assert!(weak.upgrade().is_none());
    assert_eq!(cx.update(|cx| cx.release_idle_caches().unwrap()).assets, 0);
}

#[gpui::test]
async fn last_window_hook_defers_release_and_rechecks_reopened_windows(cx: &mut TestAppContext) {
    let releases = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let releases = releases.clone();
        crate::install_idle_cache_release(cx, move || releases.set(releases.get() + 1));
    });
    let first = cx.add_window(|_, _| Empty);
    let second = cx.add_window(|_, _| Empty);
    let source = GatedSource::default();
    let (task, _) = cx.update(|cx| cx.fetch_asset::<GatedAsset>(&source));
    source.finish(0);
    let result = task.await;
    let weak = Arc::downgrade(&result);
    drop(result);

    assert!(cx.update(|cx| cx.release_idle_caches()).is_none());
    first.update(cx, |_, window, _| window.remove_window()).unwrap();
    assert!(weak.upgrade().is_some());

    let reopened = Rc::new(Cell::new(None));
    second
        .update(cx, |_, window, cx| {
            // The only window is temporarily taken out of its slot here.
            assert!(cx.release_idle_caches().is_none());
            window.remove_window();
            let reopened = reopened.clone();
            cx.defer(move |cx| {
                let window = cx.open_window(Default::default(), |_, cx| cx.new(|_| Empty)).unwrap();
                reopened.set(Some(window));
            });
        })
        .unwrap();
    assert!(weak.upgrade().is_some());
    assert_eq!(releases.get(), 0, "no release while a window is still open");
    let reopened = reopened
        .get()
        .expect("the queued window should open before the idle hook");
    reopened.update(cx, |_, window, _| window.remove_window()).unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(releases.get(), 1, "the release callback follows the actual release");
    assert_eq!(cx.update(|cx| cx.release_idle_caches().unwrap()).assets, 0);
}
