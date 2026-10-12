//! Event-loop types used by `fastab_desktop` after the process host moved from tao to GPUI.
//!
//! Background tasks (IPC, accessibility, tray) still post [`crate::event::Event`]s. The GPUI
//! application drains them on the AppKit main thread. This replaces `tao::event_loop::EventLoopProxy`
//! so the rest of the crate can keep calling `send_event`.

use std::fmt;
use std::sync::{Arc, Mutex, Weak};

use tao::platform::macos::ActivationPolicy;

use crate::event::Event;

/// Error returned when the GPUI host has shut down and can no longer accept events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventLoopClosed;

impl fmt::Display for EventLoopClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("event loop is closed")
    }
}

impl std::error::Error for EventLoopClosed {}

/// Cloneable sender used by IPC, tray, and platform observers.
#[derive(Clone, Debug)]
pub struct EventLoopProxy {
    tx: flume::Sender<Event>,
    pending_input: Arc<Mutex<Weak<QueuedOverlayInput>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OverlayInput {
    pub buffer: String,
    pub cwd: String,
    pub cursor: u32,
    pub session_id: uuid::Uuid,
}

/// Once taken by the host, an input is no longer eligible for queue deduplication.
#[derive(Debug)]
pub struct QueuedOverlayInput(Mutex<Option<OverlayInput>>);

impl QueuedOverlayInput {
    pub(crate) fn take(&self) -> Option<OverlayInput> {
        self.0.lock().unwrap_or_else(|err| err.into_inner()).take()
    }
}

impl EventLoopProxy {
    pub(crate) fn new(tx: flume::Sender<Event>) -> Self {
        Self {
            tx,
            pending_input: Default::default(),
        }
    }

    /// Post an event to the GPUI host.
    pub fn send_event(&self, event: Event) -> Result<(), EventLoopClosed> {
        // Every control, focus, caret, result and settings event is a barrier.
        // Serialize the barrier and enqueue with the input admission below.
        let mut pending = self.pending_input.lock().unwrap_or_else(|err| err.into_inner());
        *pending = Weak::new();
        self.tx.send(event).map_err(|_err| EventLoopClosed)
    }

    pub(crate) fn send_overlay_input(&self, input: OverlayInput) -> Result<(), EventLoopClosed> {
        let mut pending = self.pending_input.lock().unwrap_or_else(|err| err.into_inner());
        if self.tx.is_disconnected() {
            return Err(EventLoopClosed);
        }
        // Empty input is an implicit EndInput. Distinct text is never merged:
        // intermediate typing is needed to distinguish paste/history recall.
        if !input.buffer.trim().is_empty() {
            if let Some(previous) = pending.upgrade() {
                if previous.0.lock().unwrap_or_else(|err| err.into_inner()).as_ref() == Some(&input) {
                    return Ok(());
                }
            }
        }
        let queued = Arc::new(QueuedOverlayInput(Mutex::new(Some(input))));
        *pending = Arc::downgrade(&queued);
        self.tx
            .send(Event::GpuiOverlayInput(queued))
            .map_err(|_err| EventLoopClosed)
    }
}

/// Stand-in for tao's window-target. Dashboard windows are created with AppKit directly; this
/// type only carries activation-policy changes onto `NSApplication`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventLoopWindowTarget;

impl EventLoopWindowTarget {
    /// Apply an `NSApplicationActivationPolicy` at runtime.
    #[cfg(target_os = "macos")]
    pub fn set_activation_policy_at_runtime(&self, policy: ActivationPolicy) {
        crate::platform::set_activation_policy(policy);
    }

    #[cfg(not(target_os = "macos"))]
    pub fn set_activation_policy_at_runtime(&self, _policy: ActivationPolicy) {}
}

pub(crate) fn channel() -> (EventLoopProxy, flume::Receiver<Event>) {
    let (tx, rx) = flume::unbounded();
    (EventLoopProxy::new(tx), rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str) -> OverlayInput {
        OverlayInput {
            buffer: text.into(),
            cwd: "/tmp".into(),
            cursor: text.len() as u32,
            session_id: uuid::Uuid::nil(),
        }
    }

    #[test]
    fn only_identical_unconsumed_inputs_share_a_queue_slot() {
        let (proxy, rx) = channel();
        for _ in 0..1_000 {
            proxy.send_overlay_input(input("git ")).unwrap();
        }
        assert_eq!(rx.len(), 1);
        let Event::GpuiOverlayInput(queued) = rx.recv().unwrap() else {
            panic!("input")
        };
        assert_eq!(queued.take(), Some(input("git ")));
        // Holding the event alive after consumption must not swallow a retry.
        proxy.send_overlay_input(input("git ")).unwrap();
        proxy.send_overlay_input(input("git c")).unwrap();
        proxy.send_overlay_input(input("git ch")).unwrap();
        proxy.send_overlay_input(input("git checkout")).unwrap();
        assert_eq!(rx.len(), 4, "typing and paste transitions remain in order");
    }

    #[test]
    fn controls_empty_inputs_and_session_changes_are_barriers() {
        let (proxy, rx) = channel();
        proxy.send_overlay_input(input("git ")).unwrap();
        proxy
            .send_event(Event::GpuiOverlayEndInput {
                session_id: uuid::Uuid::nil(),
            })
            .unwrap();
        proxy.send_overlay_input(input("git ")).unwrap();
        proxy.send_overlay_input(input(" ")).unwrap();
        proxy.send_overlay_input(input(" ")).unwrap();
        proxy.send_overlay_input(input("git ")).unwrap();
        let mut other = input("git ");
        other.session_id = uuid::Uuid::new_v4();
        proxy.send_overlay_input(other).unwrap();
        assert_eq!(rx.len(), 7);
        assert!(matches!(rx.recv().unwrap(), Event::GpuiOverlayInput(_)));
        assert!(matches!(rx.recv().unwrap(), Event::GpuiOverlayEndInput { .. }));
        drop(rx);
        assert_eq!(proxy.send_overlay_input(input("other")), Err(EventLoopClosed));
    }

    #[test]
    fn a_received_but_unconsumed_input_cannot_hide_a_closed_host() {
        let (proxy, rx) = channel();
        proxy.send_overlay_input(input("git ")).unwrap();
        let queued = rx.recv().unwrap();
        drop(rx);
        assert_eq!(proxy.send_overlay_input(input("git ")), Err(EventLoopClosed));
        drop(queued);
    }
}
