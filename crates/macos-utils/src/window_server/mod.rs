#![allow(non_upper_case_globals)]

mod ax_observer;
mod ui_element;
use std::boxed::Box;
use std::ffi::c_void;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use accessibility_sys::{
    AXError, AXIsProcessTrusted, AXObserverRef, AXUIElementRef, kAXApplicationActivatedNotification,
    kAXApplicationShownNotification, kAXErrorNoValue, kAXFocusedUIElementChangedNotification,
    kAXFocusedWindowChangedNotification, kAXMainWindowChangedNotification, kAXUIElementDestroyedNotification,
    kAXWindowCreatedNotification, kAXWindowMovedNotification, kAXWindowResizedNotification, pid_t,
};
use ax_observer::AXObserver;
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use dashmap::DashMap;
use flume::Sender;
use objc2::mutability::InteriorMutable;
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2::{ClassType, DeclaredClass, declare_class, msg_send_id, sel};
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidLaunchApplicationNotification,
    NSWorkspaceDidTerminateApplicationNotification,
};
use objc2_foundation::{NSBundle, NSNotification, NSObject};
use tracing::{debug, error, info, trace, warn};
pub use ui_element::{AX_MESSAGING_TIMEOUT_SECONDS, CGWindowLevelForKey, UIElement};

use crate::util::NotificationCenter;
use crate::util::notification_center::get_app_from_notification;

// The last three are this app and the upstream builds it descends from, listed for the
// same reason `own_bundle_id` exists. They stay hardcoded because `own_bundle_id` reports
// nothing for a dev build launched outside an app bundle, and because this crate sits
// below `fastab_util` in the dependency graph so it cannot read `APP_BUNDLE_ID`.
const BLOCKED_BUNDLE_IDS: &[&str] = &[
    "com.apple.ViewBridgeAuxiliary",
    "com.apple.notificationcenterui",
    "com.apple.WebKit.WebContent",
    "com.apple.WebKit.Networking",
    "com.apple.controlcenter",
    "app.fastab",
    "dev.emmmm.easy-complete",
    "com.mschrage.fig",
    "com.amazon.codewhisperer",
];

/// The overlay raises the same accessibility notifications a terminal does, so observing
/// this process makes showing the overlay look like the user focusing another app, which
/// immediately hides it again. Resolved at runtime so that renaming the app keeps working
/// even if the hardcoded entry in [`BLOCKED_BUNDLE_IDS`] goes stale.
fn own_bundle_id() -> Option<&'static str> {
    static OWN_BUNDLE_ID: OnceLock<Option<String>> = OnceLock::new();
    OWN_BUNDLE_ID
        .get_or_init(|| {
            let bundle = NSBundle::mainBundle();
            // SAFETY: reads an immutable property of the process's own bundle.
            unsafe { bundle.bundleIdentifier() }.map(|id| id.to_string())
        })
        .as_deref()
}

/// Electron hosts that only expose their DOM to accessibility once asked, which is what
/// `find_x_term_caret_tree` needs to locate the xterm.js caret.
///
/// Duplicates `fastab_util::Terminal::is_xterm`, which cannot be used here: `fastab_util`
/// depends on this crate, so the reverse would be a cycle. Keep the two in sync.
pub const XTERM_BUNDLE_IDS: &[&str] = &[
    "com.microsoft.VSCodeInsiders",
    "com.microsoft.VSCode",
    "com.vscodium",
    "com.visualstudio.code.oss",
    "com.todesktop.230313mzl4w4u92",
    "com.todesktop.23052492jqa5xjo",
    "com.exafunction.windsurf",
    "com.exafunction.windsurf-next",
    "com.trae.app",
    "co.zeit.hyper",
    "org.tabby",
    // OpenAI Codex, shipped as ChatGPT.app since the rename.
    "com.openai.codex",
];

const TRACKED_NOTIFICATIONS: &[&str] = &[
    kAXWindowCreatedNotification,
    kAXFocusedWindowChangedNotification,
    kAXMainWindowChangedNotification,
    kAXApplicationShownNotification,
    kAXApplicationActivatedNotification,
    kAXWindowResizedNotification,
    kAXWindowMovedNotification,
    kAXUIElementDestroyedNotification,
];

/// Electron terminals put the terminal, the editor and the sidebar in one window, so moving
/// between them changes neither the focused window nor the active app and fires none of
/// [`TRACKED_NOTIFICATIONS`] — the overlay would sit there until something else happened to hide
/// it. Otty's native terminal text area also needs this when Find or another pane takes focus.
/// Keep other IME-only native terminals off this chatty notification.
const XTERM_TRACKED_NOTIFICATIONS: &[&str] = &[kAXFocusedUIElementChangedNotification];

/// Which AX notifications to subscribe for an app. Kept separate from the subscribe loop so the
/// element-level notification cannot quietly grow to every tracked app.
fn tracked_notifications(bundle_id: &str) -> Vec<&'static str> {
    let extra: &[&str] = if XTERM_BUNDLE_IDS.contains(&bundle_id) || bundle_id == "io.appmakes.otty" {
        XTERM_TRACKED_NOTIFICATIONS
    } else {
        &[]
    };

    TRACKED_NOTIFICATIONS.iter().chain(extra).copied().collect()
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct ApplicationSpecifier {
    pub pid: pid_t,
    pub bundle_id: String,
}

fn frontmost_matches(pid: pid_t, bundle_id: Option<&str>, expected: &ApplicationSpecifier) -> bool {
    pid == expected.pid && bundle_id == Some(expected.bundle_id.as_str())
}

/// Whether a delayed accessibility event still belongs to the frontmost app.
/// Call before making any AX requests for an event from a previously active app.
pub fn is_frontmost_application(expected: &ApplicationSpecifier) -> bool {
    let Some(frontmost) = frontmost_application() else {
        return false;
    };
    frontmost_matches(frontmost.pid, Some(&frontmost.bundle_id), expected)
}

/// Read the current app identity without querying its accessibility tree.
pub fn frontmost_application() -> Option<ApplicationSpecifier> {
    // Focus notifications are checked on Tokio workers as well as the UI
    // thread. Only Rust-owned identity data escapes this synchronous pool.
    autoreleasepool(|_| {
        let frontmost = unsafe { NSWorkspace::sharedWorkspace().frontmostApplication() }?;
        Some(ApplicationSpecifier {
            pid: unsafe { frontmost.processIdentifier() },
            bundle_id: unsafe { frontmost.bundleIdentifier() }?.to_string(),
        })
    })
}

fn window_destroyed_after_focus_error(app: ApplicationSpecifier, error: AXError) -> Option<WindowServerEvent> {
    // NoValue explicitly says the application has no focused window. A timeout,
    // disabled AX access, or a stale element says nothing about window lifetime.
    if error == kAXErrorNoValue {
        Some(WindowServerEvent::WindowDestroyed { app })
    } else {
        debug!(ax_error = error, bundle_id = %app.bundle_id, "Keeping focused window after failed AX query");
        None
    }
}

pub enum WindowServerEvent {
    FocusChanged {
        window: UIElement,
        app: ApplicationSpecifier,
    },
    /// Keyboard focus moved to a different element inside the same window. Only emitted for
    /// [`XTERM_BUNDLE_IDS`]; see [`XTERM_TRACKED_NOTIFICATIONS`].
    FocusedElementChanged {
        element: UIElement,
        app: ApplicationSpecifier,
    },
    WindowDestroyed {
        app: ApplicationSpecifier,
    },
    ActiveSpaceChanged {
        is_fullscreen: bool,
    },
    RequestCaretPositionUpdate {
        app: ApplicationSpecifier,
        window_id: u32,
    },
}

pub struct AccessibilityCallbackData {
    pub app: ApplicationSpecifier,
    pub sender: Sender<WindowServerEvent>,
    /// Web content re-announces focus for an element that already had it. Remembering the last
    /// one keeps those repeats from hiding the overlay while the user is still typing into it.
    pub last_focused_element: Option<UIElement>,
}

unsafe fn app_bundle_id(app: &NSRunningApplication) -> Option<String> {
    app.bundleIdentifier().map(|s| s.to_string())
}

pub struct WindowServer {
    _inner: Pin<Box<WindowServerInner>>,
    observer: Retained<ObserverClass>,
}

// SAFETY: observer id pointer is send + sync safe
unsafe impl Send for WindowServer {}
unsafe impl Sync for WindowServer {}

pub struct WindowServerInner {
    observers: DashMap<ApplicationSpecifier, AXObserver<AccessibilityCallbackData>, fnv::FnvBuildHasher>,
    sender: Sender<WindowServerEvent>,
    activation_queries: ActivationQueries,
}

type ActivationQuery = dyn Fn(&ApplicationSpecifier) -> Option<WindowServerEvent> + Send + Sync;

struct ActivationRequest {
    app: ApplicationSpecifier,
    generation: u64,
    deadline: Instant,
}

#[derive(Default)]
struct ActivationState {
    generation: u64,
    current: Option<ApplicationSpecifier>,
    pending: Option<ActivationRequest>,
    running: bool,
    closed: bool,
}

#[derive(Default)]
struct ActivationShared {
    state: Mutex<ActivationState>,
    changed: Condvar,
}

/// One real native call at a time, with only the newest delayed request retained.
/// Cancelling/replacing a request never releases the worker slot while AX is blocked.
struct ActivationQueries {
    shared: Arc<ActivationShared>,
    sender: Sender<WindowServerEvent>,
    query: Arc<ActivationQuery>,
}

impl ActivationQueries {
    fn new(sender: Sender<WindowServerEvent>) -> Self {
        Self::with_query(sender, query_activated_window)
    }

    fn with_query(
        sender: Sender<WindowServerEvent>,
        query: impl Fn(&ApplicationSpecifier) -> Option<WindowServerEvent> + Send + Sync + 'static,
    ) -> Self {
        Self {
            shared: Arc::default(),
            sender,
            query: Arc::new(query),
        }
    }

    fn cancel(&self, only_app: Option<&ApplicationSpecifier>) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        if only_app.is_some_and(|app| state.current.as_ref() != Some(app)) {
            return;
        }
        state.generation = state.generation.wrapping_add(1);
        state.current = None;
        state.pending = None;
        self.shared.changed.notify_one();
    }

    fn schedule(&self, app: ApplicationSpecifier, delay: Duration) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        if state.closed {
            return;
        }
        state.generation = state.generation.wrapping_add(1);
        state.current = Some(app.clone());
        state.pending = Some(ActivationRequest {
            app,
            generation: state.generation,
            deadline: Instant::now() + delay,
        });
        self.shared.changed.notify_one();
        if state.running {
            return;
        }
        state.running = true;
        let shared = self.shared.clone();
        let sender = self.sender.clone();
        let query = self.query.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("fastab-ax-activation".into())
            .spawn(move || activation_worker(shared, sender, query))
        {
            state.running = false;
            warn!(%error, "Could not start delayed AX query worker");
        }
    }
}

impl Drop for ActivationQueries {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        state.closed = true;
        state.generation = state.generation.wrapping_add(1);
        state.pending = None;
        state.current = None;
        self.shared.changed.notify_one();
    }
}

fn activation_worker(shared: Arc<ActivationShared>, sender: Sender<WindowServerEvent>, query: Arc<ActivationQuery>) {
    loop {
        let request = {
            let mut state = shared.state.lock().unwrap_or_else(|err| err.into_inner());
            loop {
                let deadline = state.pending.as_ref().map(|request| request.deadline);
                let Some(deadline) = deadline.filter(|_| !state.closed) else {
                    // Clear the slot under the same mutex used by schedule: a new
                    // activation cannot be stranded between this check and exit.
                    state.running = false;
                    shared.changed.notify_all();
                    return;
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break state.pending.take().expect("pending deadline belongs to a request");
                }
                (state, _) = shared
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|err| err.into_inner());
            }
        };
        let current = || {
            let state = shared.state.lock().unwrap_or_else(|err| err.into_inner());
            !state.closed && state.generation == request.generation
        };
        if !current() {
            continue;
        }
        // The synchronous leaf checks the frontmost identity both before and
        // after AX. It never runs on either Tokio worker or the AppKit thread.
        if let Some(event) = query(&request.app) {
            let state = shared.state.lock().unwrap_or_else(|err| err.into_inner());
            if !state.closed && state.generation == request.generation {
                // The production channel is unbounded. Do not block under the
                // scheduling mutex even if a caller supplies a bounded channel.
                if let Err(error) = sender.try_send(event) {
                    warn!(%error, "Could not send delayed AX focus event");
                }
            }
        }
    }
}

fn query_activated_window(app: &ApplicationSpecifier) -> Option<WindowServerEvent> {
    autoreleasepool(|_| {
        if !is_frontmost_application(app) {
            return None;
        }
        let result = UIElement::application(app.pid).focused_window();
        if !is_frontmost_application(app) {
            return None;
        }
        match result {
            Ok(window) => Some(WindowServerEvent::FocusChanged {
                window,
                app: app.clone(),
            }),
            Err(error) => {
                warn!(?error, bundle_id = %app.bundle_id, "Could not read focused window after activation");
                None
            },
        }
    })
}

impl WindowServer {
    /// Native callbacks cannot block their run loop to await the desktop.
    /// Expose the existing FIFO ownership rather than silently discarding
    /// focus transitions when an arbitrary bounded channel would be full.
    pub fn pending_events(&self) -> usize {
        self._inner.sender.len()
    }

    pub fn new(sender: Sender<WindowServerEvent>) -> Self {
        let (mut inner, observer) = WindowServerInner::new_with_observer(sender);

        let mut center = NotificationCenter::workspace_center();

        // Previously (in Swift) subscribed to the following as no-ops / log only:
        // - NSWorkspaceDidDeactivateApplicationNotification
        unsafe {
            center.subscribe_with_observer(
                NSWorkspaceActiveSpaceDidChangeNotification,
                &observer,
                sel!(activeSpaceChanged:),
            );

            center.subscribe_with_observer(
                NSWorkspaceDidLaunchApplicationNotification,
                &observer,
                sel!(didLaunchApplication:),
            );

            center.subscribe_with_observer(
                NSWorkspaceDidTerminateApplicationNotification,
                &observer,
                sel!(didTerminateApplication:),
            );

            center.subscribe_with_observer(
                NSWorkspaceDidActivateApplicationNotification,
                &observer,
                sel!(didActivateApplication:),
            );
        }

        inner.init();
        Self {
            _inner: inner,
            observer,
        }
    }
}

impl Drop for WindowServer {
    fn drop(&mut self) {
        let center = NotificationCenter::workspace_center();
        unsafe {
            center.remove_observer(&self.observer);
        }
    }
}

trait WindowServerHandler {
    fn did_activate_application(&mut self, notif: &NSNotification);
    fn active_space_changed(&mut self, notif: &NSNotification);
    fn did_terminate_application(&mut self, notif: &NSNotification);
    fn did_launch_application(&mut self, notif: &NSNotification);
}

const OBSERVER_CLASS_NAME: &str = "CodeWhisperer_WindowServerObserver";

pub struct Ivars {
    handler: *mut c_void,
}

declare_class! {
    pub struct ObserverClass;

    // - The superclass NSObject does not have any subclassing requirements.
    // - Interior mutability is a safe default.
    // - `ObserverClass` does not implement `Drop`.
    unsafe impl ClassType for ObserverClass {
        type Super = NSObject;
        type Mutability = InteriorMutable;
        const NAME: &'static str = OBSERVER_CLASS_NAME;
    }

    impl DeclaredClass for ObserverClass {
        type Ivars = Ivars;
    }

    unsafe impl ObserverClass {
        #[method_id(initWithHandler:)]
        fn init_with_handler(this: Allocated<Self>, handler: *mut c_void) -> Option<Retained<Self>> {
            let this = this.set_ivars(Ivars {
                handler
            });
            unsafe { msg_send_id![super(this), init] }
        }

        #[method(didActivateApplication:)]
        fn did_activate_application(&self, notif: &NSNotification) {
            let inner = self.ivars().handler as *mut WindowServerInner;
            let inner = unsafe { &mut *inner };
            inner.did_activate_application(notif);
        }

        #[method(activeSpaceChanged:)]
        fn active_space_changed(&self, notif: &NSNotification) {
            let inner = self.ivars().handler as *mut WindowServerInner;
            let inner = unsafe { &mut *inner };
            inner.active_space_changed(notif);
        }

        #[method(didTerminateApplication:)]
        fn did_terminate_application(&self, notif: &NSNotification) {
            let inner = self.ivars().handler as *mut WindowServerInner;
            let inner = unsafe { &mut *inner };
            inner.did_terminate_application(notif);
        }

        #[method(didLaunchApplication:)]
        fn did_launch_application(&self, notif: &NSNotification) {
            let inner = self.ivars().handler as *mut WindowServerInner;
            let inner = unsafe { &mut *inner };
            inner.did_launch_application(notif);
        }
    }
}

impl ObserverClass {
    pub fn new(handler: *mut c_void) -> Retained<Self> {
        unsafe { msg_send_id![Self::alloc(), initWithHandler:handler] }
    }
}

impl WindowServerInner {
    pub fn new(sender: Sender<WindowServerEvent>) -> Self {
        Self {
            observers: Default::default(),
            activation_queries: ActivationQueries::new(sender.clone()),
            sender,
        }
    }

    pub fn new_with_observer(sender: Sender<WindowServerEvent>) -> (Pin<Box<Self>>, Retained<ObserverClass>) {
        let pin = Box::pin(Self::new(sender));
        let handler = &*pin as *const Self as *mut c_void;
        let r = ObserverClass::new(handler);
        (pin, r)
    }

    #[allow(clippy::missing_safety_doc)]
    unsafe fn register(&mut self, ns_app: &NSRunningApplication, from_activation: bool) {
        if from_activation {
            // A blocked/unobservable app still supersedes the previous focus.
            self.activation_queries.cancel(None);
        }
        if !AXIsProcessTrusted() {
            info!("Cannot register to observer window events without accessibility perms");
            return;
        }

        let bundle_id = match app_bundle_id(ns_app) {
            Some(bundle_id) => bundle_id,
            None => {
                debug!("Ignoring empty bundle id");
                return;
            },
        };

        if own_bundle_id() == Some(bundle_id.as_str()) {
            debug!("Ignoring own process {bundle_id:?}");
            return;
        }

        for blocked_bundle in BLOCKED_BUNDLE_IDS {
            if *blocked_bundle == bundle_id {
                debug!("Ignoring bundle id {:?}", bundle_id);
                return;
            }
        }

        if ns_app.activationPolicy() == NSApplicationActivationPolicy::Prohibited {
            debug!("Ignoring application by activation policy");
            return;
        }

        let pid = ns_app.processIdentifier();
        let key = ApplicationSpecifier {
            pid,
            bundle_id: bundle_id.clone(),
        };

        let app_element = UIElement::application(pid);

        if self.observers.contains_key(&key) {
            debug!("app {} is already registered", key.bundle_id);
            self.deregister(&key)
        }

        if from_activation {
            self.activation_queries
                .schedule(key.clone(), Duration::from_millis(250));
        }

        let is_xterm = XTERM_BUNDLE_IDS.contains(&key.bundle_id.as_str());
        if is_xterm {
            app_element.enable_screen_reader_accessibility().ok();
        }

        let bundle_id = key.bundle_id.clone();
        let mut observer = match AXObserver::create(
            key.pid,
            app_element,
            AccessibilityCallbackData {
                app: key.clone(),
                sender: self.sender.clone(),
                last_focused_element: None,
            },
            application_ax_callback,
        ) {
            Ok(observer) => observer,
            Err(err) => {
                warn!(?err, "Could not create accessibility observer for {bundle_id:?}");
                return;
            },
        };

        // Terminals that do not use AppKit views reject some of these, and an app that
        // rejects one still delivers the rest. Subscribing all-or-nothing would leave
        // such a terminal with no window tracking at all, which strands the overlay at
        // whichever window was focused before it.
        let mut subscribed = Vec::new();
        let mut rejected: Vec<(&str, AXError)> = Vec::new();
        for notification in tracked_notifications(&key.bundle_id) {
            match observer.subscribe(notification) {
                Ok(()) => subscribed.push(notification),
                Err(err) => rejected.push((notification, err)),
            }
        }

        if !rejected.is_empty() {
            warn!(?rejected, "Notifications rejected by {bundle_id:?}");
        }

        if subscribed.is_empty() {
            warn!("Error setting up tracking for '{bundle_id:?}': every notification was rejected");
            return;
        }

        debug!(?subscribed, "Began tracking {bundle_id:?}");
        self.observers.insert(key, observer);
    }

    fn deregister(&mut self, app: &ApplicationSpecifier) {
        self.observers.remove(app);
    }

    fn register_all(&mut self) {
        self.deregister_all();

        autoreleasepool(|_| unsafe {
            let workspace = NSWorkspace::sharedWorkspace();
            if let Some(app) = workspace.frontmostApplication() {
                self.register(&app, true);
            }

            for app in workspace.runningApplications().iter() {
                self.register(app, false)
            }
        });

        info!("Tracking {:?} applications", self.observers.len());
    }

    pub fn init(&mut self) {
        self.register_all();
    }

    fn deregister_all(&mut self) {
        self.activation_queries.cancel(None);
        self.observers.clear();
    }
}

impl WindowServerHandler for WindowServerInner {
    fn did_activate_application(&mut self, notif: &NSNotification) {
        unsafe {
            if let Some(app) = get_app_from_notification(notif) {
                let bundle_id = app_bundle_id(&app);
                trace!("Activated application {bundle_id:?}");
                self.register(&app, true);
            }
        }
    }

    fn active_space_changed(&mut self, notif: &NSNotification) {
        unsafe {
            let Some(object) = notif.object() else { return };
            let workspace: Retained<NSWorkspace> = Retained::<AnyObject>::cast(object);
            let Some(app) = workspace.frontmostApplication() else {
                return;
            };
            let app_elem = UIElement::application(app.processIdentifier());
            if let Ok(window) = app_elem.focused_window() {
                let fullscreen = window.is_fullscreen();
                if let Ok(is_fullscreen) = fullscreen {
                    if let Err(e) = self
                        .sender
                        .send(WindowServerEvent::ActiveSpaceChanged { is_fullscreen })
                    {
                        warn!("Error sending active space changed notif: {e:?}");
                    }
                }
            }
        }
    }

    fn did_terminate_application(&mut self, notif: &NSNotification) {
        unsafe {
            if let Some(ns_app) = get_app_from_notification(notif) {
                if let Some(bundle_id) = app_bundle_id(&ns_app) {
                    let app = ApplicationSpecifier {
                        pid: ns_app.processIdentifier(),
                        bundle_id,
                    };
                    trace!(?app, "Deregistering terminated application instance");
                    self.activation_queries.cancel(Some(&app));
                    // The map is keyed by process, not just bundle. Keeping a
                    // sibling alive must not keep this dead instance's AX
                    // observer, element and run-loop source alive with it.
                    self.deregister(&app);
                }
            }
        }
    }

    fn did_launch_application(&mut self, notif: &NSNotification) {
        unsafe {
            if let Some(app) = get_app_from_notification(notif) {
                let bundle_id = app_bundle_id(&app);
                trace!("Launched application - {bundle_id:?}");
                // A background launch must not replace the current activation
                // timer. Its activation notification schedules the focus query.
                self.register(&app, false)
            }
        }
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn application_ax_callback(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    notification_name: CFStringRef,
    refcon: *mut c_void,
) {
    if refcon.is_null() {
        error!("refcon must not be null");
        return;
    }

    let cb_data: &mut AccessibilityCallbackData = &mut *(refcon as *mut AccessibilityCallbackData);
    // get_rule will call CFRetain to increment the RC in objc to make sure element is not freed
    // before we are done with it. CFRelease is called automatically on drop.
    let element = UIElement::from(element);

    let name = CFString::wrap_under_get_rule(notification_name);
    let app = cb_data.app.clone();

    let event_name = name.to_string();

    let event = match &*event_name {
        kAXFocusedWindowChangedNotification | kAXMainWindowChangedNotification => {
            Some(WindowServerEvent::FocusChanged {
                window: element,
                app: app.clone(),
            })
        },
        kAXApplicationActivatedNotification | kAXApplicationShownNotification => {
            if is_frontmost_application(&app) {
                element
                    .focused_window()
                    .ok()
                    .map(|window| WindowServerEvent::FocusChanged {
                        window,
                        app: app.clone(),
                    })
            } else {
                None
            }
        },
        kAXFocusedUIElementChangedNotification => {
            if is_frontmost_application(&app) && cb_data.last_focused_element.as_ref() == Some(&element) {
                None
            } else {
                cb_data.last_focused_element = Some(element.clone());
                Some(WindowServerEvent::FocusedElementChanged {
                    element,
                    app: app.clone(),
                })
            }
        },
        kAXWindowResizedNotification | kAXWindowMovedNotification => {
            if is_frontmost_application(&app) {
                element
                    .window_id_before(std::time::Instant::now() + std::time::Duration::from_millis(250))
                    .ok()
                    .map(|window_id| WindowServerEvent::RequestCaretPositionUpdate {
                        app: app.clone(),
                        window_id,
                    })
            } else {
                None
            }
        },
        kAXUIElementDestroyedNotification => {
            // Destruction notifications also describe child elements. Clear the
            // tracked window only when AX explicitly reports no focused window.
            if is_frontmost_application(&app) {
                match UIElement::application(app.pid).focused_window() {
                    Ok(_) => None,
                    Err(err) => window_destroyed_after_focus_error(app.clone(), err),
                }
            } else {
                None
            }
        },

        unknown => {
            info!("Unhandled AX event: {unknown}");
            None
        },
    };

    if let Some(event) = event {
        if let Err(e) = cb_data.sender.send(event) {
            warn!("Error sending focus changed event: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activation_app(pid: pid_t) -> ApplicationSpecifier {
        ApplicationSpecifier {
            pid,
            bundle_id: "app.fastab.activation-test".into(),
        }
    }

    fn wait_for_activation_idle(shared: &ActivationShared) {
        let state = shared.state.lock().unwrap();
        let (state, _) = shared
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| state.running)
            .unwrap();
        assert!(!state.running, "activation worker did not retire");
    }

    fn receive_activation(receiver: &flume::Receiver<WindowServerEvent>, pid: pid_t) {
        let event = receiver.recv_timeout(Duration::from_secs(5)).expect("activation event");
        assert!(matches!(event, WindowServerEvent::WindowDestroyed { app } if app.pid == pid));
    }

    #[test]
    fn delayed_activations_coalesce_and_the_idle_worker_restarts() {
        let (sender, receiver) = flume::unbounded();
        let owner_thread = std::thread::current().id();
        let queries = ActivationQueries::with_query(sender, move |app| {
            assert_ne!(std::thread::current().id(), owner_thread);
            Some(WindowServerEvent::WindowDestroyed { app: app.clone() })
        });
        queries.schedule(activation_app(1), Duration::from_secs(60));
        queries.schedule(activation_app(2), Duration::ZERO);
        receive_activation(&receiver, 2);
        wait_for_activation_idle(&queries.shared);
        queries.schedule(activation_app(3), Duration::ZERO);
        receive_activation(&receiver, 3);
        wait_for_activation_idle(&queries.shared);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn blocked_activation_keeps_its_slot_and_discards_the_superseded_reply() {
        let (sender, receiver) = flume::unbounded();
        let (started_tx, started_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::unbounded::<()>();
        let queries = ActivationQueries::with_query(sender, move |app| {
            let _ = started_tx.send(app.pid);
            if app.pid == 1 {
                let _ = release_rx.recv();
            }
            Some(WindowServerEvent::WindowDestroyed { app: app.clone() })
        });
        queries.schedule(activation_app(1), Duration::ZERO);
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        queries.schedule(activation_app(2), Duration::ZERO);
        queries.schedule(activation_app(3), Duration::ZERO);
        assert!(started_rx.try_recv().is_err());
        // Dropping also releases the blocked closure if an assertion above fails.
        drop(release_tx);
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 3);
        receive_activation(&receiver, 3);
        wait_for_activation_idle(&queries.shared);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn activation_cancellation_matches_the_instance_and_drop_rejects_inflight_results() {
        let (sender, receiver) = flume::unbounded();
        let (started_tx, started_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::unbounded();
        let queries = ActivationQueries::with_query(sender, move |app| {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Some(WindowServerEvent::WindowDestroyed { app: app.clone() })
        });
        queries.schedule(activation_app(1), Duration::ZERO);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        queries.cancel(Some(&activation_app(2)));
        release_tx.send(()).unwrap();
        receive_activation(&receiver, 1);
        wait_for_activation_idle(&queries.shared);

        queries.schedule(activation_app(1), Duration::ZERO);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        queries.cancel(Some(&activation_app(1)));
        release_tx.send(()).unwrap();
        wait_for_activation_idle(&queries.shared);
        assert!(receiver.try_recv().is_err());

        queries.schedule(activation_app(1), Duration::ZERO);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let shared = queries.shared.clone();
        drop(queries);
        release_tx.send(()).unwrap();
        wait_for_activation_idle(&shared);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn deregistering_one_instance_releases_its_observer_and_preserves_its_sibling() {
        use core_foundation::base::CFGetRetainCount;

        let pid = std::process::id() as pid_t;
        let terminated = ApplicationSpecifier {
            pid,
            bundle_id: "app.fastab.observer-test".into(),
        };
        let sibling = ApplicationSpecifier {
            pid: pid + 1,
            bundle_id: terminated.bundle_id.clone(),
        };
        let (sender, _receiver) = flume::unbounded();
        let mut server = WindowServerInner::new(sender.clone());
        let terminated_element = UIElement::application(pid);
        let sibling_element = UIElement::application(pid);
        let retain_count = |element: &UIElement| unsafe { CFGetRetainCount(element.get_ref().cast()) };
        let terminated_baseline = retain_count(&terminated_element);
        let sibling_baseline = retain_count(&sibling_element);

        for (app, element) in [(&terminated, &terminated_element), (&sibling, &sibling_element)] {
            // Use real native observers without subscribing to another app's
            // AX tree. The logical instance keys differ; both native observer
            // fixtures watch this process and need no Accessibility grant.
            let observer = unsafe {
                AXObserver::create(
                    pid,
                    element.clone(),
                    AccessibilityCallbackData {
                        app: app.clone(),
                        sender: sender.clone(),
                        last_focused_element: None,
                    },
                    application_ax_callback,
                )
            }
            .expect("create an observer for the test process");
            server.observers.insert(app.clone(), observer);
        }
        assert_eq!(retain_count(&terminated_element), terminated_baseline + 1);
        assert_eq!(retain_count(&sibling_element), sibling_baseline + 1);

        server.deregister(&terminated);
        assert!(!server.observers.contains_key(&terminated));
        assert!(server.observers.contains_key(&sibling));
        assert_eq!(retain_count(&terminated_element), terminated_baseline);
        assert_eq!(retain_count(&sibling_element), sibling_baseline + 1);

        // Duplicate termination notifications are harmless; the remaining
        // instance stays observed until its own lifetime ends.
        server.deregister(&terminated);
        assert_eq!(server.observers.len(), 1);
        drop(server);
        assert_eq!(retain_count(&sibling_element), sibling_baseline);
    }

    #[test]
    fn failed_ax_queries_do_not_destroy_the_focused_window() {
        let app = ApplicationSpecifier {
            pid: 42,
            bundle_id: "io.appmakes.otty".into(),
        };
        for error in [
            accessibility_sys::kAXErrorCannotComplete,
            accessibility_sys::kAXErrorInvalidUIElement,
            accessibility_sys::kAXErrorAPIDisabled,
            accessibility_sys::kAXErrorFailure,
        ] {
            assert!(window_destroyed_after_focus_error(app.clone(), error).is_none());
        }
        assert!(matches!(
            window_destroyed_after_focus_error(app.clone(), kAXErrorNoValue),
            Some(WindowServerEvent::WindowDestroyed { app: destroyed }) if destroyed == app
        ));
    }

    #[test]
    fn element_focus_tracks_electron_and_native_ax_terminals_but_not_ime_only_terminals() {
        let vscode = tracked_notifications("com.microsoft.VSCode");
        assert!(vscode.contains(&kAXFocusedUIElementChangedNotification));
        // Otty exposes a native terminal text area; opening Find or switching
        // panes must invalidate its caret even without a shell-buffer update.
        assert!(tracked_notifications("io.appmakes.otty").contains(&kAXFocusedUIElementChangedNotification));

        // Native terminals switch windows, not panes, and the notification is noisy enough that
        // subscribing it everywhere risks hiding the overlay mid-keystroke.
        let ghostty = tracked_notifications("com.mitchellh.ghostty");
        assert!(!ghostty.contains(&kAXFocusedUIElementChangedNotification));
    }

    #[test]
    fn window_level_notifications_are_tracked_everywhere() {
        for bundle_id in ["com.microsoft.VSCode", "com.mitchellh.ghostty", "com.apple.Terminal"] {
            let tracked = tracked_notifications(bundle_id);
            for notification in TRACKED_NOTIFICATIONS {
                assert!(tracked.contains(notification), "{bundle_id} is missing {notification}");
            }
        }
    }

    #[test]
    fn frontmost_filter_matches_both_process_and_bundle_identity() {
        let observed = ApplicationSpecifier {
            pid: 42,
            bundle_id: "com.microsoft.VSCode".into(),
        };

        assert!(frontmost_matches(42, Some("com.microsoft.VSCode"), &observed));
        assert!(!frontmost_matches(43, Some("com.microsoft.VSCode"), &observed));
        assert!(!frontmost_matches(42, Some("com.other.Editor"), &observed));
        assert!(!frontmost_matches(42, None, &observed));
    }
}
