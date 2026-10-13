//! macOS overlay hardening: floating level, join all spaces, non-activating.

#![allow(unexpected_cfgs)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSArray, NSPoint, NSRect, NSString};
use objc::rc::autoreleasepool;
use objc::{class, msg_send, sel, sel_impl};

/// `NSWindowStyleMaskNonactivatingPanel` — overlay must not steal key focus.
const NS_WINDOW_STYLE_NONACTIVATING_PANEL: u64 = 1 << 7;
/// `NSWindowAnimationBehaviorNone`
const NS_WINDOW_ANIMATION_BEHAVIOR_NONE: i64 = 2;

const NS_FLOATING_WINDOW_LEVEL: i64 = 3;
const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: u64 = 1 << 0;
const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: u64 = 1 << 8;
const NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY: u64 = 1 << 4;

/// Title used to find the GPUI overlay `NSWindow` without touching the dashboard.
pub const OVERLAY_WINDOW_TITLE: &str = "FasTab Autocomplete";

/// Invalidates queued frame requests when the overlay is hidden and lets only
/// the newest position request bring the singleton overlay window forward.
static OVERLAY_FRAME_EPOCH: AtomicU64 = AtomicU64::new(0);
static OVERLAY_FRAME_DRAIN_SCHEDULED: AtomicBool = AtomicBool::new(false);

const MAX_OVERLAY_FRAME_RETRIES: u8 = 4;
const FRAME_EPS: f64 = 0.5;
/// One display frame at 60Hz. Long enough for GPUI's foreground executor to
/// apply `setContentSize`, short enough that a corrected frame is not visible.
#[cfg(target_os = "macos")]
const OVERLAY_FRAME_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(16);

#[derive(Clone, Copy)]
struct OverlayFrameRequest {
    window: usize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    epoch: u64,
    retries: u8,
}

impl OverlayFrameRequest {
    fn same_geometry(&self, other: &Self) -> bool {
        overlay_geometry_close(
            (self.window, self.x, self.y, self.width, self.height),
            (other.window, other.x, other.y, other.width, other.height),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverlayFrameSchedule {
    /// Same caret frame as the request already in flight or just applied.
    /// Must not bump [`OVERLAY_FRAME_EPOCH`]: size retries keep the original
    /// epoch, and `overlay_frame_request_is_current` is an equality check.
    IgnoreEcho,
    /// A genuinely new placement. Caller bumps the epoch and enqueues.
    Enqueue,
}

struct OverlayFrameQueue {
    pending: Option<OverlayFrameRequest>,
    applied: Option<OverlayFrameRequest>,
}

fn decide_overlay_frame_schedule(
    next: &OverlayFrameRequest,
    pending: Option<&OverlayFrameRequest>,
    applied: Option<&OverlayFrameRequest>,
) -> OverlayFrameSchedule {
    if pending.is_some_and(|current| current.same_geometry(next)) {
        return OverlayFrameSchedule::IgnoreEcho;
    }
    // `drain` publishes `applied` before `apply`. The AppKit write can echo
    // through GPUI after a newer request has already bumped the epoch and
    // taken `pending`. Matching `applied` — not its epoch — is what keeps
    // that echo from overwriting the newer frame. Hide-then-reshow works
    // because `invalidate_overlay_frame_requests` clears `applied`.
    if applied.is_some_and(|current| current.same_geometry(next)) {
        return OverlayFrameSchedule::IgnoreEcho;
    }
    OverlayFrameSchedule::Enqueue
}

fn retain_pending_after_apply(
    applied: OverlayFrameRequest,
    pending: Option<OverlayFrameRequest>,
) -> Option<OverlayFrameRequest> {
    match pending {
        Some(next) if next.same_geometry(&applied) => None,
        other => other,
    }
}

fn overlay_geometry_close(left: (usize, f64, f64, f64, f64), right: (usize, f64, f64, f64, f64)) -> bool {
    left.0 == right.0
        && (left.1 - right.1).abs() < FRAME_EPS
        && (left.2 - right.2).abs() < FRAME_EPS
        && (left.3 - right.3).abs() < FRAME_EPS
        && (left.4 - right.4).abs() < FRAME_EPS
}

fn overlay_frame_queue() -> &'static Mutex<OverlayFrameQueue> {
    static QUEUE: OnceLock<Mutex<OverlayFrameQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        Mutex::new(OverlayFrameQueue {
            pending: None,
            applied: None,
        })
    })
}

fn begin_overlay_frame_request() -> u64 {
    OVERLAY_FRAME_EPOCH.fetch_add(1, Ordering::SeqCst) + 1
}

fn invalidate_overlay_frame_requests() {
    OVERLAY_FRAME_EPOCH.fetch_add(1, Ordering::SeqCst);
    let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
    queue.pending = None;
    queue.applied = None;
}

fn overlay_frame_request_is_current(epoch: u64) -> bool {
    OVERLAY_FRAME_EPOCH.load(Ordering::SeqCst) == epoch
}

fn for_each_window_titled(title: Option<&str>, mut f: impl FnMut(id)) {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = title;
        let _ = &mut f;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        autoreleasepool(|| {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            if app == nil {
                return;
            }
            let windows: id = msg_send![app, windows];
            if windows == nil {
                return;
            }
            let wanted: id = match title {
                Some(title) => {
                    let s: id = NSString::alloc(nil).init_str(title);
                    let _: id = msg_send![s, autorelease];
                    s
                },
                None => nil,
            };
            let count = NSArray::count(windows);
            for i in 0..count {
                let window: id = windows.objectAtIndex(i);
                if wanted != nil {
                    let ns_title: id = msg_send![window, title];
                    if ns_title == nil {
                        continue;
                    }
                    let equal: bool = msg_send![ns_title, isEqualToString: wanted];
                    if !equal {
                        continue;
                    }
                }
                f(window);
            }
        });
    }
}

fn window_titled(title: &str) -> Option<id> {
    let mut found = None;
    for_each_window_titled(Some(title), |window| {
        if found.is_none() {
            found = Some(window);
        }
    });
    found
}

fn set_layer_square(view: id) {
    if view == nil {
        return;
    }
    unsafe {
        let _: () = msg_send![view, setWantsLayer: YES];
        let layer: id = msg_send![view, layer];
        if layer != nil {
            let _: () = msg_send![layer, setCornerRadius: 0.0f64];
            let _: () = msg_send![layer, setMasksToBounds: NO];
        }
    }
}

/// Borderless panel, floating level, join-all-spaces. Call once after the window is created.
/// Calling `setStyleMask` after `setFrame` reintroduces AppKit's titled-panel chrome.
fn configure_overlay_window(window: id) {
    unsafe {
        let _: () = msg_send![window, setLevel: NS_FLOATING_WINDOW_LEVEL];
        let behavior: u64 = NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
            | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY
            | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY;
        let _: () = msg_send![window, setCollectionBehavior: behavior];
        let _: () = msg_send![window, setHidesOnDeactivate: NO];
        let _: () = msg_send![window, setIgnoresMouseEvents: NO];
        let _: () = msg_send![window, setStyleMask: NS_WINDOW_STYLE_NONACTIVATING_PANEL];
        let _: () = msg_send![window, setAnimationBehavior: NS_WINDOW_ANIMATION_BEHAVIOR_NONE];
    }
    polish_overlay_window(window);
}

/// `setStyleMask:` may synchronously emit resize/move notifications. GPUI
/// installs callbacks for those notifications which re-enter its `AppCell`, so
/// applying the native window policy from inside `Entity::update` can fail with
/// `already borrowed` and leave the renderer's viewport stale. Queue it onto
/// the next main-dispatch turn, after the current GPUI update has returned.
#[cfg(target_os = "macos")]
fn schedule_configure_overlay_window(window: id) {
    let window = window as usize;
    dispatch::Queue::main().exec_async(move || {
        autoreleasepool(|| configure_overlay_window(window as id));
    });
}

#[cfg(not(target_os = "macos"))]
fn schedule_configure_overlay_window(window: id) {
    let _ = window;
}

/// No AppKit shadow / rounded theme-frame. Safe to call on every show; does not change style mask.
fn polish_overlay_window(window: id) {
    unsafe {
        let _: () = msg_send![window, setHasShadow: NO];
        let _: () = msg_send![window, setOpaque: NO];
        let clear: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![window, setBackgroundColor: clear];
        let content: id = msg_send![window, contentView];
        set_layer_square(content);
        if content != nil {
            let superview: id = msg_send![content, superview];
            set_layer_square(superview);
        }
    }
}

/// Apply overlay window level / space behavior to every NSWindow (spike) or a titled window.
#[cfg(target_os = "macos")]
pub fn harden_overlay_window() {
    harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
}

#[cfg(not(target_os = "macos"))]
pub fn harden_overlay_window() {}

pub fn harden_overlay_window_titled(title: &str) {
    let filter = if title.is_empty() { None } else { Some(title) };
    for_each_window_titled(filter, |window| {
        configure_overlay_window(window);
    });
}

pub fn polish_overlay_window_titled(title: &str) {
    let filter = if title.is_empty() { None } else { Some(title) };
    for_each_window_titled(filter, |window| {
        polish_overlay_window(window);
    });
}

pub fn set_overlay_window_level(level: i64) {
    set_overlay_window_level_for_title(OVERLAY_WINDOW_TITLE, level);
}

pub fn set_overlay_window_level_for_title(title: &str, level: i64) {
    for_each_window_titled(Some(title), |window| unsafe {
        let _: () = msg_send![window, setLevel: level];
    });
}

pub fn set_overlay_visible_titled(title: &str, visible: bool) {
    if visible {
        for_each_window_titled(Some(title), order_overlay_front);
    } else {
        park_overlay_window_titled(title);
    }
}

fn order_overlay_front(window: id) {
    unsafe {
        let _: () = msg_send![window, orderFrontRegardless];
    }
}

fn order_overlay_out(window: id) {
    unsafe {
        let _: () = msg_send![window, orderOut: cocoa::base::nil];
    }
}

/// Defer the AppKit position/show operation until GPUI has released `App`.
/// Content sizing is deliberately handled by `gpui::Window::resize`; changing it
/// through AppKit here makes GPUI's synchronous resize callback re-borrow `App`,
/// which can leave the renderer at a stale viewport and eventually starve events.
fn schedule_overlay_frame(window: id, x: f64, y: f64, width: f64, height: f64) {
    #[cfg(target_os = "macos")]
    {
        let next = OverlayFrameRequest {
            window: window as usize,
            x,
            y,
            width,
            height,
            epoch: 0,
            retries: 0,
        };
        let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
        // Layout can request the same caret frame again while the AppKit apply
        // or GPUI size retry is still in flight. Bumping the epoch for that
        // duplicate would cancel `retry_overlay_frame_after`, which is the
        // only thing that re-pins the caret edge after GPUI's deferred resize.
        if decide_overlay_frame_schedule(&next, queue.pending.as_ref(), queue.applied.as_ref())
            == OverlayFrameSchedule::IgnoreEcho
        {
            return;
        }
        let epoch = begin_overlay_frame_request();
        queue.pending = Some(OverlayFrameRequest { epoch, ..next });
        drop(queue);
        if !OVERLAY_FRAME_DRAIN_SCHEDULED.swap(true, Ordering::AcqRel) {
            queue_overlay_frame_drain_later();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, x, y, width, height);
    }
}

/// Never `exec_async` this onto the main queue. GCD keeps popping blocks that
/// were queued from a block already running on `_dispatch_main_queue_drain`, so
/// a duplicate request that `decide_overlay_frame_schedule` does not recognise
/// re-enters the drain inside the same turn and starves NSApplication — the
/// overlay then holds whatever it last painted, which is the `···` marker.
#[cfg(target_os = "macos")]
fn queue_overlay_frame_drain_later() {
    dispatch::Queue::main().exec_after(OVERLAY_FRAME_RETRY_DELAY, drain_overlay_frame);
}

#[cfg(target_os = "macos")]
fn drain_overlay_frame() {
    // Publish `applied` in the same lock that clears `pending`. Otherwise an
    // echo that arrives after `take` and before `apply` looks like a new
    // request, bumps the epoch, and cancels the size-retry chain.
    let request = {
        let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
        match queue.pending.take() {
            Some(request) if overlay_frame_request_is_current(request.epoch) => {
                queue.applied = Some(request);
                Some(request)
            },
            _ => None,
        }
    };
    if let Some(request) = request {
        apply_overlay_frame(
            request.window as id,
            request.x,
            request.y,
            request.width,
            request.height,
            request.epoch,
            request.retries,
        );
        // A layout pass during apply can write the same geometry back into
        // `pending`. Drop it instead of treating it as a new caret request.
        let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
        queue.pending = retain_pending_after_apply(request, queue.pending.take());
    }

    OVERLAY_FRAME_DRAIN_SCHEDULED.store(false, Ordering::Release);
    let has_newer = overlay_frame_queue()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .pending
        .is_some();
    if has_newer
        && OVERLAY_FRAME_DRAIN_SCHEDULED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        // Coalesce a burst of newer layout requests long enough for GPUI's
        // deferred content resize to land before applying the next frame, and
        // yield the main queue so the drain cannot feed itself.
        queue_overlay_frame_drain_later();
    }
}

#[cfg(target_os = "macos")]
fn retry_overlay_frame_after(window: id, x: f64, y: f64, width: f64, height: f64, epoch: u64, retries: u8) {
    let window = window as usize;
    dispatch::Queue::main().exec_after(OVERLAY_FRAME_RETRY_DELAY, move || {
        if overlay_frame_request_is_current(epoch) {
            apply_overlay_frame(window as id, x, y, width, height, epoch, retries);
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn retry_overlay_frame_after(window: id, x: f64, y: f64, width: f64, height: f64, epoch: u64, retries: u8) {
    let _ = (window, x, y, width, height, epoch, retries);
}

fn frame_top_left_close(frame: NSRect, x: f64, top_left_y: f64) -> bool {
    (frame.origin.x - x).abs() < FRAME_EPS && (frame.origin.y + frame.size.height - top_left_y).abs() < FRAME_EPS
}

fn frame_size_close(frame: NSRect, width: f64, height: f64) -> bool {
    (frame.size.width - width).abs() < FRAME_EPS && (frame.size.height - height).abs() < FRAME_EPS
}

/// Only the caret edge needs an AppKit write. Size mismatch is GPUI's deferred
/// `setContentSize` and is waited out by [`overlay_frame_should_retry`]. Writing
/// `setFrameTopLeftPoint` for a size-only mismatch emits an unnecessary move
/// notification and can cancel the resize retry with another layout pass.
fn overlay_frame_needs_reposition(top_left_matches: bool) -> bool {
    !top_left_matches
}

/// GPUI defers `setContentSize` onto the foreground executor. If we pin the
/// top-left before that resize lands, AppKit keeps the bottom-left origin and
/// the caret-relative edge drifts — most visibly when the list shrinks.
fn overlay_frame_should_retry(size_matches: bool, retries: u8) -> bool {
    !size_matches && retries < MAX_OVERLAY_FRAME_RETRIES
}

/// Apply the Quartz top-left position and show the already GPUI-sized window.
fn apply_overlay_frame(window: id, x: f64, y: f64, width: f64, height: f64, epoch: u64, retries: u8) {
    let top_left_y = match primary_screen_cocoa() {
        Some((origin_y, primary_height)) => quartz_y_to_cocoa_frame_y(y, 0.0, origin_y, primary_height),
        None => y,
    };
    unsafe {
        let current: NSRect = msg_send![window, frame];
        let visible: bool = msg_send![window, isVisible];
        let size_matches = frame_size_close(current, width, height);
        // Re-pin only when the caret edge actually moved. After GPUI's
        // deferred resize lands, the next retry sees a drifted top-left and
        // pins then — AppKit keeps the bottom-left origin, so the caret
        // edge is stale once the list shrinks.
        if overlay_frame_needs_reposition(frame_top_left_close(current, x, top_left_y)) {
            let _: () = msg_send![window, setFrameTopLeftPoint: NSPoint::new(x, top_left_y)];
        }
        if !visible {
            order_overlay_front(window);
        }
        if overlay_frame_should_retry(size_matches, retries) {
            // Keep the original epoch so a newer user request can still cancel us.
            // Re-dispatching immediately would burn every retry inside the same
            // runloop turn, before GPUI's deferred resize can land.
            retry_overlay_frame_after(window, x, y, width, height, epoch, retries + 1);
        }
    }
}

/// `NSWindow` backing a GPUI window. Title lookup misses hidden / never-shown panels.
pub fn ns_window_from_gpui(window: &gpui::Window) -> Option<id> {
    #[cfg(target_os = "macos")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::AppKit(appkit) => {
                let view = appkit.ns_view.as_ptr() as id;
                if view == nil {
                    return None;
                }
                unsafe {
                    let ns_window: id = msg_send![view, window];
                    if ns_window == nil { None } else { Some(ns_window) }
                }
            },
            _ => None,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        None
    }
}

pub fn harden_overlay_window_handle(window: &gpui::Window) {
    if let Some(ns_window) = ns_window_from_gpui(window) {
        schedule_configure_overlay_window(ns_window);
    } else {
        #[cfg(target_os = "macos")]
        dispatch::Queue::main().exec_async(|| harden_overlay_window_titled(OVERLAY_WINDOW_TITLE));
        #[cfg(not(target_os = "macos"))]
        harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
    }
}

pub fn park_overlay_window_handle(window: &gpui::Window) {
    invalidate_overlay_frame_requests();
    if let Some(ns_window) = ns_window_from_gpui(window) {
        order_overlay_out(ns_window);
    } else {
        order_overlay_out_titled(OVERLAY_WINDOW_TITLE);
    }
}

pub fn set_overlay_visible_handle(window: &gpui::Window, visible: bool) {
    if let Some(ns_window) = ns_window_from_gpui(window) {
        if visible {
            order_overlay_front(ns_window);
        } else {
            invalidate_overlay_frame_requests();
            order_overlay_out(ns_window);
        }
    } else {
        set_overlay_visible_titled(OVERLAY_WINDOW_TITLE, visible);
    }
}

pub fn set_overlay_frame_handle(window: &gpui::Window, x: f64, y: f64, width: f64, height: f64) {
    let _ = try_set_overlay_frame_handle(window, x, y, width, height);
}

pub(crate) fn try_set_overlay_frame_handle(window: &gpui::Window, x: f64, y: f64, width: f64, height: f64) -> bool {
    if let Some(ns_window) = ns_window_from_gpui(window).or_else(|| window_titled(OVERLAY_WINDOW_TITLE)) {
        // Epoch is assigned only if this geometry is actually enqueued.
        // Duplicates of the live frame must not bump it or in-flight size
        // retries (`retry_overlay_frame_after`) fail the equality check.
        schedule_overlay_frame(ns_window, x, y, width, height);
        true
    } else {
        // Do not turn a missing native window into an asynchronous no-op while
        // the GPUI caller records the frame as positioned. Keeping the request
        // uncommitted lets the same geometry retry on the next layout pass and
        // keeps terminal interception disabled until a window is available.
        false
    }
}

/// Hide the overlay the way the WebView host did: `orderOut`, keep the last size.
pub fn park_overlay_window_titled(title: &str) {
    invalidate_overlay_frame_requests();
    order_overlay_out_titled(title);
}

fn order_overlay_out_titled(title: &str) {
    for_each_window_titled(Some(title), order_overlay_out);
}

/// Convert a global Quartz (top-left, origin at the top of the primary display)
/// Y into a Cocoa `NSWindow` frame origin Y (bottom-left of the primary display).
pub fn quartz_y_to_cocoa_frame_y(quartz_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - quartz_y - height
}

/// The display that anchors the global coordinate space: `NSScreen.screens[0]`,
/// the one carrying the menu bar, whose Cocoa origin is `(0, 0)` and whose top
/// edge is Quartz `y = 0`.
///
/// Deliberately **not** `NSScreen.mainScreen`, which is whichever display holds
/// the key window and therefore moves with focus. On a single display the two
/// agree; with an external monitor `mainScreen` shifts the flip anchor by the
/// difference between the two screens' top edges and the overlay lands at the
/// wrong height.
#[cfg(target_os = "macos")]
fn primary_screen() -> Option<id> {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens == nil || NSArray::count(screens) == 0 {
            return None;
        }
        let primary: id = screens.objectAtIndex(0);
        if primary == nil { None } else { Some(primary) }
    }
}

fn primary_screen_cocoa() -> Option<(f64, f64)> {
    #[cfg(target_os = "macos")]
    unsafe {
        let screen = primary_screen()?;
        let frame: NSRect = msg_send![screen, frame];
        Some((frame.origin.y, frame.size.height))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Flip one Cocoa screen frame (bottom-left origin) into the Quartz rect space
/// that Accessibility caret coordinates use (top-left origin, `y` growing down
/// from the primary display's top edge).
fn cocoa_screen_to_quartz(frame: (f64, f64, f64, f64), primary_top: f64) -> (f64, f64, f64, f64) {
    let (x, y, width, height) = frame;
    (x, primary_top - (y + height), width, height)
}

/// Screens as Quartz rects `(x, y, width, height)` with origin at the top-left of
/// the primary display — the same space Accessibility caret coordinates use.
pub fn screens_quartz() -> Vec<(f64, f64, f64, f64)> {
    #[cfg(target_os = "macos")]
    unsafe {
        autoreleasepool(|| {
            let screens: id = msg_send![class!(NSScreen), screens];
            if screens == nil {
                return Vec::new();
            }
            let Some(primary) = primary_screen() else {
                return Vec::new();
            };
            let primary_frame: NSRect = msg_send![primary, frame];
            let primary_top = primary_frame.origin.y + primary_frame.size.height;
            let count = NSArray::count(screens);
            let mut out = Vec::with_capacity(count as usize);
            let mut push_screen = |screen: id| {
                let frame: NSRect = msg_send![screen, frame];
                out.push(cocoa_screen_to_quartz(
                    (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height),
                    primary_top,
                ));
            };

            // The primary display stays first. IME caret coordinates arrive in
            // Cocoa's global bottom-left space, whose Y conversion is anchored
            // to this screen rather than whichever display holds the caret, and
            // callers read `screens[0]` for that anchor.
            push_screen(primary);
            for i in 0..count {
                let screen: id = screens.objectAtIndex(i);
                if screen != primary {
                    push_screen(screen);
                }
            }
            out
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// Place the overlay in global Quartz (top-left) coordinates by converting to AppKit's
/// top-left point, matching tao `set_outer_position`.
pub fn set_overlay_frame_titled(title: &str, x: f64, y: f64, width: f64, height: f64) {
    let epoch = begin_overlay_frame_request();
    let title = title.to_string();
    #[cfg(target_os = "macos")]
    dispatch::Queue::main().exec_after(OVERLAY_FRAME_RETRY_DELAY, move || {
        if overlay_frame_request_is_current(epoch) {
            apply_overlay_frame_titled(&title, x, y, width, height, epoch);
        }
    });
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, x, y, width, height, epoch);
    }
}

fn apply_overlay_frame_titled(title: &str, x: f64, y: f64, width: f64, height: f64, epoch: u64) {
    for_each_window_titled(Some(title), |window| {
        apply_overlay_frame(window, x, y, width, height, epoch, 0);
    });
}

/// Best-effort read of the process-wide AppKit appearance. Defaults to dark when
/// NSApplication is not ready yet (common during early startup).
pub fn system_appearance_is_dark() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        autoreleasepool(|| {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            if app == nil {
                return true;
            }
            let appearance: id = msg_send![app, effectiveAppearance];
            if appearance == nil {
                return true;
            }
            let name: id = msg_send![appearance, name];
            if name == nil {
                return true;
            }
            let dark: id = NSString::alloc(nil).init_str("NSAppearanceNameDarkAqua");
            let _: id = msg_send![dark, autorelease];
            let vibrant: id = NSString::alloc(nil).init_str("NSAppearanceNameVibrantDark");
            let _: id = msg_send![vibrant, autorelease];
            let is_dark: bool = msg_send![name, isEqualToString: dark];
            let is_vibrant: bool = msg_send![name, isEqualToString: vibrant];
            is_dark || is_vibrant
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use cocoa::foundation::{NSPoint, NSRect, NSSize};

    use super::{
        OverlayFrameRequest, OverlayFrameSchedule, begin_overlay_frame_request, cocoa_screen_to_quartz,
        decide_overlay_frame_schedule, frame_size_close, frame_top_left_close, invalidate_overlay_frame_requests,
        overlay_frame_needs_reposition, overlay_frame_request_is_current, overlay_frame_should_retry,
        overlay_geometry_close, quartz_y_to_cocoa_frame_y, retain_pending_after_apply,
    };

    #[test]
    fn only_latest_unhidden_frame_request_can_show() {
        let old = begin_overlay_frame_request();
        let latest = begin_overlay_frame_request();
        assert!(!overlay_frame_request_is_current(old));
        assert!(overlay_frame_request_is_current(latest));
        invalidate_overlay_frame_requests();
        assert!(!overlay_frame_request_is_current(latest));
    }

    #[test]
    fn quartz_to_cocoa_uses_primary_display_height() {
        // 900px primary, caret 100px from the top, 140px overlay → Cocoa origin 660.
        assert_eq!(quartz_y_to_cocoa_frame_y(100.0, 140.0, 0.0, 900.0), 660.0);
    }

    #[test]
    fn quartz_to_cocoa_top_left_matches_tao_set_frame_top_left() {
        // Quartz y=100 on a 900px primary → Cocoa top-left y=800.
        assert_eq!(quartz_y_to_cocoa_frame_y(100.0, 0.0, 0.0, 900.0), 800.0);
    }

    #[test]
    fn quartz_to_cocoa_accounts_for_primary_origin() {
        assert_eq!(quartz_y_to_cocoa_frame_y(50.0, 20.0, 200.0, 800.0), 930.0);
    }

    #[test]
    fn frame_top_left_comparison_tolerates_subpixel_jitter() {
        let a = NSRect::new(NSPoint::new(10.0, 20.0), NSSize::new(320.0, 140.0));
        assert!(frame_top_left_close(a, 10.2, 160.2));
        assert!(!frame_top_left_close(a, 40.0, 160.0));
    }

    #[test]
    fn frame_size_comparison_tolerates_subpixel_jitter() {
        let a = NSRect::new(NSPoint::new(10.0, 20.0), NSSize::new(320.0, 140.0));
        assert!(frame_size_close(a, 320.2, 139.8));
        assert!(!frame_size_close(a, 320.0, 88.0));
    }

    #[test]
    fn primary_screen_maps_onto_quartz_origin() {
        let primary = (0.0, 0.0, 1440.0, 900.0);
        assert_eq!(cocoa_screen_to_quartz(primary, 900.0), (0.0, 0.0, 1440.0, 900.0));
    }

    #[test]
    fn taller_external_monitor_reaches_above_quartz_zero() {
        // 1440x900 laptop as primary, a 2560x1440 display to its right and
        // bottom-aligned with it: Cocoa (1440, 0). Its top edge sits 540px above
        // the primary's, which is negative Y in the caret's Quartz space.
        let external = (1440.0, 0.0, 2560.0, 1440.0);
        assert_eq!(
            cocoa_screen_to_quartz(external, 900.0),
            (1440.0, -540.0, 2560.0, 1440.0)
        );
    }

    #[test]
    fn external_monitor_below_the_primary_gets_positive_quartz_y() {
        // Display stacked under the laptop: its top edge starts where the
        // primary's bottom edge ends.
        let below = (0.0, -1080.0, 1920.0, 1080.0);
        assert_eq!(cocoa_screen_to_quartz(below, 900.0), (0.0, 900.0, 1920.0, 1080.0));
    }

    #[test]
    fn quartz_flip_is_anchored_to_the_primary_not_the_focused_screen() {
        // Caret 200px down the global Quartz space with a 140px overlay. The
        // answer must come from the 900px primary; anchoring to the bottom-
        // aligned 1440px external display that holds the key window would drop
        // the overlay 540px too low.
        assert_eq!(quartz_y_to_cocoa_frame_y(200.0, 140.0, 0.0, 900.0), 560.0);
        assert_eq!(quartz_y_to_cocoa_frame_y(200.0, 140.0, 0.0, 1440.0), 1100.0);
    }

    #[test]
    fn overlay_retries_until_gpui_resize_lands() {
        assert!(overlay_frame_should_retry(false, 0));
        assert!(overlay_frame_should_retry(false, 3));
        assert!(!overlay_frame_should_retry(false, 4));
        assert!(!overlay_frame_should_retry(true, 0));
    }

    #[test]
    fn size_only_mismatch_does_not_write_the_appkit_frame() {
        // GPUI's deferred resize is waited out. Touching AppKit for a size-only
        // mismatch adds a move notification without correcting the caret edge.
        assert!(!overlay_frame_needs_reposition(true));
        assert!(overlay_frame_needs_reposition(false));
    }

    fn sample_request(x: f64, y: f64) -> OverlayFrameRequest {
        OverlayFrameRequest {
            window: 1,
            x,
            y,
            width: 320.0,
            height: 140.0,
            epoch: 7,
            retries: 0,
        }
    }

    #[test]
    fn identical_geometry_is_not_a_newer_frame_request() {
        let applied = sample_request(10.0, 20.0);
        let echo = OverlayFrameRequest {
            epoch: 8,
            retries: 0,
            ..applied
        };
        assert!(applied.same_geometry(&echo));
        assert!(retain_pending_after_apply(applied, Some(echo)).is_none());
        assert!(overlay_geometry_close(
            (1, 10.0, 20.0, 320.0, 140.0),
            (1, 10.2, 20.4, 320.3, 139.8)
        ));
        assert!(!overlay_geometry_close(
            (1, 10.0, 20.0, 320.0, 140.0),
            (1, 40.0, 20.0, 320.0, 140.0)
        ));
    }

    #[test]
    fn identical_geometry_on_a_recreated_window_is_enqueued() {
        let applied = sample_request(10.0, 20.0);
        let replacement = OverlayFrameRequest {
            window: 2,
            epoch: 0,
            ..applied
        };
        assert_eq!(
            decide_overlay_frame_schedule(&replacement, None, Some(&applied)),
            OverlayFrameSchedule::Enqueue
        );
    }

    #[test]
    fn a_moved_caret_still_replaces_the_pending_frame() {
        let applied = sample_request(10.0, 20.0);
        let moved = sample_request(10.0, 80.0);
        assert_eq!(
            retain_pending_after_apply(applied, Some(moved)).map(|r| r.y),
            Some(80.0)
        );
    }

    #[test]
    fn echo_of_the_live_applied_frame_does_not_advance_epoch() {
        // After drain takes `pending`, apply's AppKit write echoes the same
        // caret frame. That echo must not look like a newer request: retries
        // keep the applied epoch, and `overlay_frame_request_is_current` is
        // an equality check.
        let applied = sample_request(10.0, 20.0);
        let echo = OverlayFrameRequest {
            epoch: 0,
            retries: 0,
            ..applied
        };
        assert_eq!(
            decide_overlay_frame_schedule(&echo, None, Some(&applied)),
            OverlayFrameSchedule::IgnoreEcho
        );
    }

    #[test]
    fn echo_matching_pending_geometry_is_ignored() {
        let pending = sample_request(10.0, 20.0);
        let echo = OverlayFrameRequest {
            epoch: pending.epoch + 1,
            ..pending
        };
        assert_eq!(
            decide_overlay_frame_schedule(&echo, Some(&pending), None),
            OverlayFrameSchedule::IgnoreEcho
        );
    }

    #[test]
    fn hide_then_reshow_same_geometry_is_a_new_request() {
        let applied = sample_request(10.0, 20.0);
        let again = OverlayFrameRequest { epoch: 0, ..applied };
        // `invalidate_overlay_frame_requests` clears `applied`. Same geometry
        // is only an echo while that slot still holds the live frame.
        assert_eq!(
            decide_overlay_frame_schedule(&again, None, None),
            OverlayFrameSchedule::Enqueue
        );
    }

    #[test]
    fn echo_of_applied_does_not_clobber_a_newer_pending_frame() {
        let applied = sample_request(10.0, 20.0);
        let newer = sample_request(10.0, 80.0);
        let echo = OverlayFrameRequest { epoch: 0, ..applied };
        // B already bumped the epoch. The in-flight apply of A can still echo;
        // treating that as Enqueue would overwrite B, then retain would drop it.
        assert_eq!(
            decide_overlay_frame_schedule(&echo, Some(&newer), Some(&applied)),
            OverlayFrameSchedule::IgnoreEcho
        );
        assert_eq!(
            retain_pending_after_apply(applied, Some(newer)).map(|r| r.y),
            Some(80.0)
        );
    }

    #[test]
    fn the_frame_drain_never_reenters_the_main_queue_in_the_same_turn() {
        // GCD's `_dispatch_main_queue_drain` keeps popping blocks queued from a
        // block already running on that drain. Scheduling `drain_overlay_frame`
        // with `exec_async` therefore livelocks NSApplication as soon as one
        // duplicate request slips past `decide_overlay_frame_schedule`, and the
        // overlay freezes on whatever it last painted — usually the `···`
        // marker. Twice regressed; keep this assertion with the code.
        let src = include_str!("macos.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the file");
        assert!(
            !src.contains("exec_async(drain_overlay_frame)"),
            "drain_overlay_frame must be exec_after'd, never exec_async'd onto the main queue"
        );
        for site in [
            "fn schedule_overlay_frame",
            "fn drain_overlay_frame",
            "fn set_overlay_frame_titled",
        ] {
            let start = src.find(site).unwrap_or_else(|| panic!("{site} is gone"));
            let body = &src[start..];
            let end = body[1..].find("\nfn ").map_or(body.len(), |i| i + 1);
            // Prose is allowed to name the banned call; only real dispatches count.
            let code = body[..end]
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<String>();
            assert!(
                !code.contains("exec_async("),
                "{site} must yield the main queue: every frame dispatch goes through exec_after"
            );
        }
    }

    #[test]
    fn a_moved_caret_is_enqueued_even_while_a_retry_is_in_flight() {
        let applied = sample_request(10.0, 20.0);
        let moved = sample_request(10.0, 80.0);
        assert_eq!(
            decide_overlay_frame_schedule(&moved, None, Some(&applied)),
            OverlayFrameSchedule::Enqueue
        );
    }
}
