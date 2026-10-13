use std::cell::Cell;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2::mutability::InteriorMutable;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, Bool};
use objc2::{ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, sel};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSDistributedNotificationCenter, NSPoint, NSRange, NSRect, NSSize, NSString, ns_string};
use objc2_input_method_kit::{IMKInputController, IMKServer};

use crate::wire::{self, Origin};
use crate::{paths, terminals};

const INPUT_CONTROLLER_CLASS_NAME: &str = env!("InputMethodServerControllerClass");

/// Must stay equal to `fastab_util::macos::EDIT_BUFFER_UPDATED_NOTIFICATION`.
/// Distinct from Easy Complete / Amazon Q so dual-install does not poke us.
const EDIT_BUFFER_UPDATED_NOTIFICATION: &str = "app.fastab.edit_buffer_updated";

/// A remote IMK proxy may return nil here. Never assume otherwise: a panic on
/// this process's main thread kills the IME and strands every attached terminal.
fn bundle_identifier(client: &AnyObject) -> Option<String> {
    let bundle_id: Option<Retained<NSString>> = unsafe { msg_send_id![client, bundleIdentifier] };
    Some(bundle_id?.to_string())
}

/// A client that no longer backs a live window — a closed Ghostty window whose input controller
/// is still registered, for example — leaves `attributesForCharacterIndex:lineHeightRectangle:`
/// untouched, so the caret rect stays at its zeroed default. Sending that on would place the
/// overlay at the screen-space origin, which the desktop then clamps to the bottom-left corner of
/// the primary monitor — on multi-monitor setups that is a different screen than the terminal.
fn is_valid_caret_rect(rect: NSRect) -> bool {
    rect.origin.x.is_finite()
        && rect.origin.y.is_finite()
        && rect.size.width.is_finite()
        && rect.size.height.is_finite()
        && rect.size.height > 0.0
}

/// Otty / Ghostty / Kitty attach through IMK. After an IME restart or a
/// palette-input-source switch, `deactivateServer:` flips `is_active` off
/// while the terminal is still the key window — AX cannot see their caret,
/// so dropping the request here is what makes the overlay vanish mid-session.
fn client_is_key_window(client: &AnyObject) -> Option<bool> {
    let window_sel = sel!(window);
    let responds: Bool = unsafe { msg_send![client, respondsToSelector: window_sel] };
    if !responds.as_bool() {
        return None;
    }
    let window: *const AnyObject = unsafe { msg_send![client, window] };
    if window.is_null() {
        return None;
    }
    let responds: Bool = unsafe { msg_send![window, respondsToSelector: sel!(isKeyWindow)] };
    if !responds.as_bool() {
        return None;
    }
    let is_key: Bool = unsafe { msg_send![window, isKeyWindow] };
    Some(is_key.as_bool())
}

fn frontmost_bundle_identifier() -> Option<String> {
    let frontmost = unsafe { NSWorkspace::sharedWorkspace().frontmostApplication() }?;
    unsafe { frontmost.bundleIdentifier() }.map(|bundle_id| bundle_id.to_string())
}

const CARET_WRITE_TIMEOUT: Duration = Duration::from_millis(200);
/// How long the sender thread waits for another caret before giving itself up. An
/// idle input method should hold nothing but its AppKit main thread; the next
/// keystroke starts a new one.
const SENDER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// At most one pending frame, plus the one currently being sent. The worker
/// retires while holding this same state lock, so enqueue cannot lose a frame
/// between checking worker ownership and waking a thread about to exit.
#[derive(Default)]
struct CaretMailbox {
    state: Mutex<CaretMailboxState>,
    ready: Condvar,
}

#[derive(Default)]
struct CaretMailboxState {
    pending: Option<Vec<u8>>,
    worker: Option<JoinHandle<()>>,
}

static SENDER: LazyLock<Arc<CaretMailbox>> = LazyLock::new(|| Arc::new(CaretMailbox::default()));

fn enqueue_caret_frame(frame: Vec<u8>) {
    SENDER.enqueue_with(frame, |mailbox| {
        std::thread::Builder::new()
            .name("imk-caret".into())
            .spawn(move || caret_sender_loop(mailbox, SENDER_IDLE_TIMEOUT, send_caret_frame))
    });
}

impl CaretMailbox {
    fn enqueue_with(
        self: &Arc<Self>,
        frame: Vec<u8>,
        start: impl FnOnce(Arc<Self>) -> std::io::Result<JoinHandle<()>>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        // Replace before waking the worker. Slow connect/write cannot retain
        // a queue of obsolete carets; replacing also frees the old frame now.
        state.pending = Some(frame);
        if state.worker.as_ref().is_none_or(JoinHandle::is_finished) {
            match start(self.clone()) {
                Ok(worker) => state.worker = Some(worker),
                Err(err) => {
                    state.worker = None;
                    state.pending = None;
                    log_error!("could not start the caret sender thread: {err}");
                    return;
                },
            }
        }
        self.ready.notify_one();
    }

    fn next_frame(&self, idle_timeout: Duration) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let (mut state, _) = self
            .ready
            .wait_timeout_while(state, idle_timeout, |state| state.pending.is_none())
            .unwrap_or_else(|err| err.into_inner());
        if state.pending.is_none() {
            // Clear ownership atomically with the empty-slot decision. A new
            // enqueue either fills this worker's slot or starts its successor.
            state.worker = None;
            return None;
        }
        state.pending.take()
    }
}

fn caret_sender_loop(mailbox: Arc<CaretMailbox>, idle_timeout: Duration, mut send: impl FnMut(&[u8])) {
    while let Some(frame) = mailbox.next_frame(idle_timeout) {
        // Never hold the mailbox mutex across filesystem/socket work.
        send(&frame);
    }
}

fn send_caret_frame(frame: &[u8]) {
    if let Some(path) = paths::desktop_socket_path() {
        send_caret_frame_to(path, frame);
    }
}

fn send_caret_frame_to(path: &std::path::Path, frame: &[u8]) {
    // The desktop owns this socket. When it is not running there is nobody to
    // tell, so skip the connect rather than fail it on every keystroke.
    if !path.exists() {
        return;
    }

    match UnixStream::connect(path) {
        Ok(mut stream) => {
            let _ = stream.set_write_timeout(Some(CARET_WRITE_TIMEOUT));
            if let Err(err) = stream.write_all(frame) {
                log_debug!("caret frame not written: {err}");
            }
        },
        Err(err) => log_debug!("desktop socket not reachable: {err}"),
    }
}

fn requested_caret_frame(
    bundle_id: &str,
    frontmost_bundle_id: Option<&str>,
    is_active: bool,
    is_key_window: Option<bool>,
    query_rect: impl FnOnce() -> NSRect,
) -> Option<Vec<u8>> {
    if !terminals::supports_input_method(bundle_id) {
        log_debug!("Instance {bundle_id:?} is not a supported terminal, ignoring request");
        return None;
    }

    // Distributed requests reach every retained controller. A known background
    // app/window must not overwrite the focused client's caret. Some remote IMK
    // proxies expose no window, so missing focus metadata still falls back to
    // IMK's active flag rather than suppressing a normal client indefinitely.
    if frontmost_bundle_id.is_some_and(|frontmost| frontmost != bundle_id)
        || is_key_window == Some(false)
        || (!is_active && is_key_window != Some(true))
    {
        return None;
    }

    let rect = query_rect();
    if !is_valid_caret_rect(rect) {
        log_debug!("Instance {bundle_id:?} reported an invalid caret rect {rect:?}, ignoring request");
        return None;
    }

    // This is a reply, not an unsolicited position-change notification. The
    // desktop clears its caret when the shell session changes, and can restart
    // while this IMK controller remains alive. An unchanged rect is still needed
    // to restore that state; queued replies are coalesced by the sender instead.
    log_debug!("Sending cursor position for {bundle_id:?}: {rect:?}");
    Some(wire::caret_position_frame(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
        Origin::BottomLeft,
    ))
}

fn report_caret_from_client(client: &AnyObject, bundle_id: Option<&str>, is_active: bool) {
    let frontmost_bundle_id = frontmost_bundle_identifier();
    if let Some(frame) = requested_caret_frame(
        bundle_id.unwrap_or_default(),
        frontmost_bundle_id.as_deref(),
        is_active,
        client_is_key_window(client),
        || {
            let mut rect = NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    height: 0.0,
                    width: 0.0,
                },
            };
            let _: () = unsafe { msg_send![client, attributesForCharacterIndex: 0 lineHeightRectangle: &mut rect] };
            rect
        },
    ) {
        enqueue_caret_frame(frame);
    }
}

struct Ivars {
    is_active: Cell<bool>,
}

declare_class!(
    struct MyInputController;

    // - The superclass IMKInputController does not have any subclassing requirements.
    // - Interior mutability is a safe default.
    // - `MyInputController` does not implement `Drop`.
    unsafe impl ClassType for MyInputController {
        type Super = IMKInputController;
        type Mutability = InteriorMutable;
        const NAME: &'static str = INPUT_CONTROLLER_CLASS_NAME;
    }

    impl DeclaredClass for MyInputController {
        type Ivars = Ivars;
    }

    unsafe impl MyInputController {
        #[method_id(initWithServer:delegate:client:)]
        fn init_with_server_delegate_client(this: Allocated<Self>, server: Option<&IMKServer>, delegate: Option<&AnyObject>, client: Option<&AnyObject>) -> Retained<Self> {
            log_info!("INITING");
            let partial = this.set_ivars(Ivars {
                is_active: Cell::new(true),
            });
            let this: Retained<Self> = unsafe { msg_send_id![super(partial, IMKInputController::class()), initWithServer:server delegate: delegate client: client] };

            // The desktop app posts this whenever it wants the caret for the focused window.
            let center = unsafe { NSDistributedNotificationCenter::defaultCenter() };
            unsafe {
                center.addObserver_selector_name_object(
                    &this,
                    sel!(handleCursorPositionRequest:),
                    Some(ns_string!(EDIT_BUFFER_UPDATED_NOTIFICATION)),
                    None,
                );
            }

            this
        }

        #[method(activateServer:)]
        fn activate_server(&self, client: Option<&AnyObject>) {
            self.ivars().is_active.set(true);
            let Some(client) = client else {
                return;
            };

            let bundle_id = bundle_identifier(client);
            log_info!("activated server: {bundle_id:?}");

            // Used to trigger input method enabled in Alacritty
            if bundle_id.as_deref().is_some_and(terminals::is_alacritty) {
                let empty_range = NSRange::new(0, 0);
                let space_string = ns_string!(" ");
                let empty_string = ns_string!("");

                unsafe {
                    // First, setMarkedText with a non-empty string in order to enable winit IME
                    // https://github.com/rust-windowing/winit/blob/97d4c7b303bb8110df6c492f0c2327b7d5098347/src/platform_impl/macos/view.rs#L330-L337
                    let _: () = msg_send![client, setMarkedText: space_string selectionRange: empty_range replacementRange: empty_range];

                    // Then, since we don't *actually* want to be in the preedit state, set marked text to an empty
                    // string to invalidate https://github.com/rust-windowing/winit/blob/97d4c7b303bb8110df6c492f0c2327b7d5098347/src/platform_impl/macos/view.rs#L345-L351
                    let _: () = msg_send![client, setMarkedText: empty_string selectionRange: empty_range replacementRange: empty_range];
                }
            }

            report_caret_from_client(client, bundle_id.as_deref(), true);
        }

        #[method(deactivateServer:)]
        fn deactivate_server(&self, client: Option<&AnyObject>) {
            self.ivars().is_active.set(false);
            log_info!("deactivated server: {:?}", client.and_then(bundle_identifier));
        }

        #[method(handleCursorPositionRequest:)]
        fn handle_cursor_position_request(&self, _notif: Option<&AnyObject>) {
            // Nil once the client app has gone away while the controller lingers.
            let client: *mut AnyObject = unsafe { msg_send![self, client] };
            let Some(client) = (unsafe { client.as_ref() }) else {
                return;
            };
            let is_active = self.ivars().is_active.get();
            let bundle_id = bundle_identifier(client);
            report_caret_from_client(client, bundle_id.as_deref(), is_active);
        }
    }
);

pub fn connect_imkserver(name: &NSString, identifier: Option<&NSString>) {
    log_info!("connecting to imkserver");
    let server_alloc = IMKServer::alloc();
    unsafe { IMKServer::initWithName_bundleIdentifier(server_alloc, Some(name), identifier) };
    log_info!("connected to imkserver");
}

pub fn register_controller() {
    log_info!("registering {INPUT_CONTROLLER_CLASS_NAME}...");

    MyInputController::class();

    log_info!("finished registering {INPUT_CONTROLLER_CLASS_NAME}.");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        }
    }

    #[test]
    fn zeroed_caret_rect_is_rejected() {
        assert!(!is_valid_caret_rect(rect(0.0, 0.0, 0.0, 0.0)));
    }

    #[test]
    fn caret_rect_without_height_is_rejected() {
        assert!(!is_valid_caret_rect(rect(1200.0, 800.0, 8.0, 0.0)));
        assert!(!is_valid_caret_rect(rect(1200.0, 800.0, 8.0, -16.0)));
    }

    #[test]
    fn non_finite_caret_rect_is_rejected() {
        assert!(!is_valid_caret_rect(rect(f64::NAN, 800.0, 8.0, 16.0)));
        assert!(!is_valid_caret_rect(rect(1200.0, f64::INFINITY, 8.0, 16.0)));
    }

    #[test]
    fn caret_rect_on_a_secondary_monitor_is_accepted() {
        // Screens left of or below the primary one report negative Cocoa origins.
        assert!(is_valid_caret_rect(rect(-1920.0, -450.0, 0.0, 16.0)));
        assert!(is_valid_caret_rect(rect(1200.0, 800.0, 8.0, 16.0)));
    }

    #[test]
    fn requests_resend_unchanged_caret_after_desktop_state_loss() {
        let queries = Cell::new(0);
        let request = || {
            requested_caret_frame("io.appmakes.otty", Some("io.appmakes.otty"), true, Some(true), || {
                queries.set(queries.get() + 1);
                rect(100.0, 200.0, 1.0, 16.0)
            })
            .expect("every request needs a reply, even if the desktop lost its previous caret")
        };
        let before_reset = request();
        let after_reset = request();
        assert_eq!(before_reset, after_reset);
        assert_eq!(queries.get(), 2);
    }

    #[test]
    fn focus_routing_preserves_active_proxies_without_window_metadata() {
        for (active, key_window) in [(true, None), (false, Some(true))] {
            assert!(
                requested_caret_frame("io.appmakes.otty", None, active, key_window, || {
                    rect(100.0, 200.0, 1.0, 16.0)
                })
                .is_some()
            );
        }
    }

    #[test]
    fn background_controllers_cannot_answer_for_the_focused_terminal() {
        for (frontmost, active, key_window) in [
            (Some("com.mitchellh.ghostty"), true, Some(true)),
            (Some("io.appmakes.otty"), true, Some(false)),
            (Some("io.appmakes.otty"), false, None),
        ] {
            assert!(
                requested_caret_frame("io.appmakes.otty", frontmost, active, key_window, || {
                    panic!("background clients must not be asked for their caret")
                })
                .is_none()
            );
        }
    }

    #[test]
    fn blocked_caret_sender_retains_one_pending_latest_frame() {
        let mailbox = Arc::new(CaretMailbox::default());
        let (sent, frames) = mpsc::channel();
        let (unblock, blocked) = mpsc::channel();
        let (exited, exit) = mpsc::channel();
        let first = wire::caret_position_frame(1.0, 2.0, 1.0, 16.0, Origin::BottomLeft);
        let last = wire::caret_position_frame(9.0, 8.0, 1.0, 16.0, Origin::BottomLeft);
        let idle = Duration::from_millis(10);
        mailbox.enqueue_with(first.clone(), move |mailbox| {
            std::thread::Builder::new().spawn(move || {
                let mut first = true;
                caret_sender_loop(mailbox, idle, |frame| {
                    sent.send(frame.to_vec()).unwrap();
                    if first {
                        first = false;
                        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
                    }
                });
                exited.send(()).unwrap();
            })
        });
        assert_eq!(frames.recv_timeout(Duration::from_secs(2)).unwrap(), first);
        for index in 0..10_000 {
            let frame = wire::caret_position_frame(f64::from(index), 2.0, 1.0, 16.0, Origin::BottomLeft);
            mailbox.enqueue_with(frame, |_| panic!("a blocked sender must keep its worker slot"));
        }
        mailbox.enqueue_with(last.clone(), |_| panic!("the original worker still owns the send"));
        assert_eq!(mailbox.state.lock().unwrap().pending.as_ref(), Some(&last));
        unblock.send(()).unwrap();
        assert_eq!(frames.recv_timeout(Duration::from_secs(2)).unwrap(), last);
        exit.recv_timeout(Duration::from_secs(2)).unwrap();
        let state = mailbox.state.lock().unwrap();
        assert!(state.pending.is_none());
        assert!(state.worker.is_none());
    }

    #[test]
    fn caret_enqueue_and_idle_retirement_do_not_lose_the_last_frame() {
        let mailbox = Arc::new(CaretMailbox::default());
        let starts = Arc::new(AtomicUsize::new(0));
        let (sent, frames) = mpsc::channel();
        let (retired, exits) = mpsc::channel();
        let idle = Duration::from_millis(2);
        for index in 0u64..64 {
            let started = starts.clone();
            let output = sent.clone();
            let exited = retired.clone();
            mailbox.enqueue_with(index.to_le_bytes().to_vec(), move |mailbox| {
                started.fetch_add(1, Ordering::Relaxed);
                std::thread::Builder::new().spawn(move || {
                    caret_sender_loop(mailbox, idle, |frame| output.send(frame.to_vec()).unwrap());
                    let _ = exited.send(());
                })
            });
            assert_eq!(
                frames.recv_timeout(Duration::from_secs(2)).unwrap(),
                index.to_le_bytes()
            );
            // Alternate racing the idle boundary and proving an already
            // retired worker gets a successor. All traffic is fixture-only.
            if index % 2 == 0 {
                std::thread::sleep(idle);
            } else {
                while mailbox.state.lock().unwrap().worker.is_some() {
                    exits.recv_timeout(Duration::from_secs(2)).unwrap();
                }
            }
        }
        assert!(starts.load(Ordering::Relaxed) > 1);
        assert!(mailbox.state.lock().unwrap().pending.is_none());
    }

    #[test]
    fn a_failed_caret_worker_is_replaced_on_the_next_request() {
        let mailbox = Arc::new(CaretMailbox::default());
        let (failed, failure) = mpsc::channel();
        mailbox.enqueue_with(vec![1], move |mailbox| {
            std::thread::Builder::new().spawn(move || {
                caret_sender_loop(mailbox, SENDER_IDLE_TIMEOUT, |_| {
                    failed.send(()).unwrap();
                    panic!("injected caret worker failure");
                });
            })
        });
        failure.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !mailbox.state.lock().unwrap().worker.as_ref().unwrap().is_finished() {
            assert!(std::time::Instant::now() < deadline, "injected worker must exit");
            std::thread::yield_now();
        }
        let (sent, frames) = mpsc::channel();
        let frame = wire::caret_position_frame(9.0, 8.0, 1.0, 16.0, Origin::BottomLeft);
        mailbox.enqueue_with(frame.clone(), move |mailbox| {
            std::thread::Builder::new().spawn(move || {
                caret_sender_loop(mailbox, Duration::ZERO, |frame| sent.send(frame.to_vec()).unwrap());
            })
        });
        assert_eq!(frames.recv_timeout(Duration::from_secs(2)).unwrap(), frame);
    }

    #[test]
    fn caret_request_notification_is_fastab_only() {
        let sibling = ["com.amazon.", "codewhisperer", ".edit_buffer_updated"].concat();
        assert_eq!(
            EDIT_BUFFER_UPDATED_NOTIFICATION,
            fastab_util::macos::EDIT_BUFFER_UPDATED_NOTIFICATION
        );
        assert_ne!(EDIT_BUFFER_UPDATED_NOTIFICATION, sibling.as_str());
        let production = include_str!("imk.rs").split("#[cfg(test)]").next().unwrap();
        assert!(
            production.contains("ns_string!(EDIT_BUFFER_UPDATED_NOTIFICATION)"),
            "IMK observer must listen on the Fastab notification"
        );
        assert!(
            !production.contains(&sibling),
            "IMK observer must not listen on the sibling Amazon Q notification"
        );
    }

    #[test]
    fn a_missing_desktop_socket_is_not_an_error() {
        // Uses an explicit dead path: the real one may have a live desktop app
        // behind it, and a test must not feed that overlay a fake caret.
        let dir = std::env::temp_dir().join("ec-imk-test-no-such-dir");
        send_caret_frame_to(
            &dir.join("desktop.sock"),
            &wire::caret_position_frame(1.0, 2.0, 1.0, 16.0, Origin::BottomLeft),
        );
    }
}
