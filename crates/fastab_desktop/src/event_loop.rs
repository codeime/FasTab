//! Event-loop types used by `fastab_desktop` after the process host moved from tao to GPUI.
//!
//! Background tasks (IPC, accessibility, tray) still post [`crate::event::Event`]s. The GPUI
//! application drains them on the AppKit main thread. This replaces `tao::event_loop::EventLoopProxy`
//! so the rest of the crate can keep calling `send_event`.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tao::platform::macos::ActivationPolicy;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
    tx: flume::Sender<QueuedEvent>,
    pending_input: Arc<Mutex<Weak<QueuedOverlayInput>>>,
    budget: Arc<EventBudget>,
}

const ASYNC_EVENT_LIMIT: usize = 256;
// A legal 4 MiB hook can refer to a retained cwd from an earlier hook. Keep
// room for both strings and the encoded legacy notification, without shrinking
// or coalescing any distinct input to meet admission.
const ASYNC_EVENT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct EventBudget {
    closed: AtomicBool,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    async_events: AtomicUsize,
    async_bytes: AtomicUsize,
    waiters: AtomicUsize,
    waiting_bytes: AtomicUsize,
    sync_events: AtomicUsize,
    sync_bytes: AtomicUsize,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EventQueueDiagnostics {
    pub async_events: usize,
    pub async_payload_bytes: usize,
    pub async_waiters: usize,
    pub async_waiting_bytes: usize,
    pub sync_events: usize,
    pub sync_payload_bytes: usize,
}

impl EventBudget {
    fn new(slots: usize, bytes: usize) -> Self {
        Self {
            closed: AtomicBool::new(false),
            slots: Arc::new(Semaphore::new(slots)),
            bytes: Arc::new(Semaphore::new(bytes)),
            async_events: AtomicUsize::new(0),
            async_bytes: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            waiting_bytes: AtomicUsize::new(0),
            sync_events: AtomicUsize::new(0),
            sync_bytes: AtomicUsize::new(0),
        }
    }
}

/// Admission failure is explicit; callers must propagate it rather than
/// pretending an input/control was delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventAdmissionError {
    Closed,
    PayloadTooLarge,
}

impl fmt::Display for EventAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "event loop is closed",
            Self::PayloadTooLarge => "desktop event exceeds the ingress payload budget",
        })
    }
}

impl std::error::Error for EventAdmissionError {}

struct WaitingAdmission<'a> {
    budget: &'a EventBudget,
    bytes: usize,
}

impl Drop for WaitingAdmission<'_> {
    fn drop(&mut self) {
        self.budget.waiters.fetch_sub(1, Ordering::Relaxed);
        self.budget.waiting_bytes.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct EventAdmission {
    budget: Arc<EventBudget>,
    bytes: usize,
    permits: Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
}

impl Drop for EventAdmission {
    fn drop(&mut self) {
        let (events, bytes) = if self.permits.is_some() {
            (&self.budget.async_events, &self.budget.async_bytes)
        } else {
            (&self.budget.sync_events, &self.budget.sync_bytes)
        };
        events.fetch_sub(1, Ordering::Relaxed);
        bytes.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(crate) struct QueuedEvent {
    event: Event,
    admission: EventAdmission,
}

impl QueuedEvent {
    pub(crate) fn dispatch<R>(self, dispatch: impl FnOnce(Event) -> R) -> R {
        let Self {
            event,
            admission: _admission,
        } = self;
        // Keep ownership charged through dispatch, not just while in flume.
        dispatch(event)
    }
}

pub struct EventLoopReceiver {
    rx: flume::Receiver<QueuedEvent>,
    budget: Arc<EventBudget>,
    pending_input: Arc<Mutex<Weak<QueuedOverlayInput>>>,
}

impl EventLoopReceiver {
    pub(crate) async fn recv_queued(&self) -> Result<QueuedEvent, flume::RecvError> {
        self.rx.recv_async().await
    }

    #[cfg(test)]
    pub(crate) async fn recv_async(&self) -> Result<Event, flume::RecvError> {
        self.recv_queued()
            .await
            .map(|event| event.dispatch(std::convert::identity))
    }

    #[cfg(test)]
    pub(crate) fn recv(&self) -> Result<Event, flume::RecvError> {
        self.rx.recv().map(|event| event.dispatch(std::convert::identity))
    }

    #[cfg(test)]
    pub(crate) fn try_recv(&self) -> Result<Event, flume::TryRecvError> {
        self.rx.try_recv().map(|event| event.dispatch(std::convert::identity))
    }

    #[cfg(test)]
    pub(crate) fn try_iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.rx.try_iter().map(|event| event.dispatch(std::convert::identity))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rx.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }
}

impl Drop for EventLoopReceiver {
    fn drop(&mut self) {
        // flume disconnects when the final receiver drops, but sender clones
        // still own its buffered values. Serialize shutdown against every
        // enqueue, then explicitly retire those values while proxies survive.
        let mut pending = self.pending_input.lock().unwrap_or_else(|err| err.into_inner());
        self.budget.closed.store(true, Ordering::Release);
        *pending = Weak::new();
        // Wake admissions even when the queue is empty or a byte waiter holds
        // a count permit. No sender may hang after the only consumer exits.
        self.budget.slots.close();
        self.budget.bytes.close();
        for event in self.rx.try_iter() {
            drop(event);
        }
    }
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

    pub(crate) fn payload_bytes(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
            .map_or(std::mem::size_of::<Event>(), OverlayInput::payload_bytes)
    }
}

impl EventLoopProxy {
    pub(crate) fn diagnostics(&self) -> EventQueueDiagnostics {
        EventQueueDiagnostics {
            async_events: self.budget.async_events.load(Ordering::Relaxed),
            async_payload_bytes: self.budget.async_bytes.load(Ordering::Relaxed),
            async_waiters: self.budget.waiters.load(Ordering::Relaxed),
            async_waiting_bytes: self.budget.waiting_bytes.load(Ordering::Relaxed),
            sync_events: self.budget.sync_events.load(Ordering::Relaxed),
            sync_payload_bytes: self.budget.sync_bytes.load(Ordering::Relaxed),
        }
    }

    fn synchronous_admission(&self, bytes: usize) -> EventAdmission {
        self.budget.sync_events.fetch_add(1, Ordering::Relaxed);
        self.budget.sync_bytes.fetch_add(bytes, Ordering::Relaxed);
        EventAdmission {
            budget: self.budget.clone(),
            bytes,
            permits: None,
        }
    }

    async fn admit(&self, bytes: usize) -> Result<EventAdmission, EventAdmissionError> {
        if self.budget.closed.load(Ordering::Acquire) {
            return Err(EventAdmissionError::Closed);
        }
        if bytes > ASYNC_EVENT_BYTES {
            return Err(EventAdmissionError::PayloadTooLarge);
        }
        self.budget.waiters.fetch_add(1, Ordering::Relaxed);
        self.budget.waiting_bytes.fetch_add(bytes, Ordering::Relaxed);
        let _waiting = WaitingAdmission {
            budget: &self.budget,
            bytes,
        };
        // Some callers run on GPUI's foreground executor. Never consume a
        // Tokio cooperative budget there, even if the native loop is entered
        // by a future caller from inside Runtime::block_on.
        let (slot, payload) = tokio::task::unconstrained(async {
            let slot = self.budget.slots.clone().acquire_owned().await?;
            let payload = self.budget.bytes.clone().acquire_many_owned(bytes as u32).await?;
            Ok::<_, tokio::sync::AcquireError>((slot, payload))
        })
        .await
        .map_err(|_err| EventAdmissionError::Closed)?;
        self.budget.async_events.fetch_add(1, Ordering::Relaxed);
        self.budget.async_bytes.fetch_add(bytes, Ordering::Relaxed);
        Ok(EventAdmission {
            budget: self.budget.clone(),
            bytes,
            permits: Some((slot, payload)),
        })
    }

    /// Await only from an asynchronous producer that owns a bounded number of
    /// pending calls. Do not spawn one sender task per event: that merely moves
    /// an unbounded queue into waiting futures.
    pub(crate) async fn send_event_async(&self, event: Event) -> Result<(), EventAdmissionError> {
        let admission = self.admit(event.ingress_payload_bytes()).await?;
        let mut pending = self.pending_input.lock().unwrap_or_else(|err| err.into_inner());
        if self.budget.closed.load(Ordering::Acquire) {
            return Err(EventAdmissionError::Closed);
        }
        *pending = Weak::new();
        self.tx
            .send(QueuedEvent { event, admission })
            .map_err(|_err| EventAdmissionError::Closed)
    }

    pub(crate) async fn send_overlay_input_async(&self, input: OverlayInput) -> Result<(), EventAdmissionError> {
        // Preserve admission of identical pending input even when the budget
        // is full. Recheck after the await because any intervening event is a
        // barrier and the previous input may have been consumed meanwhile.
        {
            let pending = self.pending_input.lock().unwrap_or_else(|err| err.into_inner());
            if self.budget.closed.load(Ordering::Acquire) || self.tx.is_disconnected() {
                return Err(EventAdmissionError::Closed);
            }
            if !input.buffer.trim().is_empty()
                && pending.upgrade().is_some_and(|previous| {
                    previous.0.lock().unwrap_or_else(|err| err.into_inner()).as_ref() == Some(&input)
                })
            {
                return Ok(());
            }
        }
        let admission = self.admit(input.payload_bytes()).await?;
        self.enqueue_input(input, admission)
            .map_err(|_err| EventAdmissionError::Closed)
    }

    /// Post an event to the GPUI host.
    pub fn send_event(&self, event: Event) -> Result<(), EventLoopClosed> {
        // Every control, focus, caret, result and settings event is a barrier.
        // Serialize the barrier and enqueue with the input admission below.
        let mut pending = self.pending_input.lock().unwrap_or_else(|err| err.into_inner());
        if self.budget.closed.load(Ordering::Acquire) {
            return Err(EventLoopClosed);
        }
        *pending = Weak::new();
        let admission = self.synchronous_admission(event.ingress_payload_bytes());
        self.tx
            .send(QueuedEvent { event, admission })
            .map_err(|_err| EventLoopClosed)
    }

    #[cfg(test)]
    pub(crate) fn send_overlay_input(&self, input: OverlayInput) -> Result<(), EventLoopClosed> {
        let admission = self.synchronous_admission(input.payload_bytes());
        self.enqueue_input(input, admission)
    }

    fn enqueue_input(&self, input: OverlayInput, admission: EventAdmission) -> Result<(), EventLoopClosed> {
        let mut pending = self.pending_input.lock().unwrap_or_else(|err| err.into_inner());
        if self.budget.closed.load(Ordering::Acquire) || self.tx.is_disconnected() {
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
            .send(QueuedEvent {
                event: Event::GpuiOverlayInput(queued),
                admission,
            })
            .map_err(|_err| EventLoopClosed)
    }
}

impl OverlayInput {
    fn payload_bytes(&self) -> usize {
        std::mem::size_of::<Event>()
            .saturating_add(std::mem::size_of::<Self>())
            .saturating_add(self.buffer.capacity())
            .saturating_add(self.cwd.capacity())
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

pub(crate) fn channel() -> (EventLoopProxy, EventLoopReceiver) {
    let (tx, rx) = flume::unbounded();
    let budget = Arc::new(EventBudget::new(ASYNC_EVENT_LIMIT, ASYNC_EVENT_BYTES));
    let pending_input = Arc::new(Mutex::new(Weak::new()));
    (
        EventLoopProxy {
            tx,
            pending_input: pending_input.clone(),
            budget: budget.clone(),
        },
        EventLoopReceiver {
            rx,
            budget,
            pending_input,
        },
    )
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

    #[tokio::test]
    async fn ingress_holds_admission_through_dispatch_without_blocking_sync_controls() {
        let (proxy, rx) = channel();
        let end = || Event::GpuiOverlayEndInput {
            session_id: uuid::Uuid::nil(),
        };
        for _ in 0..ASYNC_EVENT_LIMIT {
            proxy.send_event_async(end()).await.unwrap();
        }
        let mut pending = Box::pin(proxy.send_event_async(end()));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        assert_eq!(proxy.diagnostics().async_waiters, 1);
        let event = rx.recv_queued().await.unwrap();
        assert!(
            futures::poll!(pending.as_mut()).is_pending(),
            "dequeue is still in flight"
        );
        event.dispatch(|_| {
            proxy.send_event(Event::ReloadAccessibility).unwrap();
            assert_eq!(proxy.diagnostics().async_events, ASYNC_EVENT_LIMIT);
            assert_eq!(proxy.diagnostics().sync_events, 1);
        });
        pending.await.unwrap();
        assert_eq!(proxy.diagnostics().async_events, ASYNC_EVENT_LIMIT);
        assert_eq!(proxy.diagnostics().async_waiters, 0);
        for _ in 1..ASYNC_EVENT_LIMIT {
            assert!(matches!(rx.recv().unwrap(), Event::GpuiOverlayEndInput { .. }));
        }
        assert!(matches!(rx.recv().unwrap(), Event::ReloadAccessibility));
        assert!(matches!(rx.recv().unwrap(), Event::GpuiOverlayEndInput { .. }));
        assert_eq!(proxy.diagnostics().async_events, 0);
        assert_eq!(proxy.diagnostics().sync_events, 0);
    }

    #[tokio::test]
    async fn ingress_bytes_cancelled_waiters_and_host_shutdown_release_ownership() {
        let (proxy, rx) = channel();
        let event = || Event::GpuiOverlayBuffer {
            buffer: String::with_capacity(ASYNC_EVENT_BYTES / 2 + 1),
            cwd: String::new(),
            cursor: 0,
            session_id: uuid::Uuid::nil(),
        };
        proxy.send_event_async(event()).await.unwrap();
        let mut cancelled = Box::pin(proxy.send_event_async(event()));
        assert!(
            futures::poll!(cancelled.as_mut()).is_pending(),
            "byte bound before count bound"
        );
        drop(cancelled);
        assert_eq!(proxy.diagnostics().async_waiters, 0);
        let mut closed = Box::pin(proxy.send_event_async(event()));
        assert!(futures::poll!(closed.as_mut()).is_pending());
        drop(rx);
        assert_eq!(closed.await, Err(EventAdmissionError::Closed));
        assert_eq!(proxy.diagnostics().async_waiters, 0);
        assert_eq!(proxy.diagnostics().async_events, 0);
        assert_eq!(proxy.diagnostics().async_payload_bytes, 0);
        assert_eq!(proxy.send_event(Event::ReloadAccessibility), Err(EventLoopClosed));
    }

    #[tokio::test]
    async fn admitted_inputs_preserve_distinct_text_empty_input_and_control_barriers() {
        let (proxy, rx) = channel();
        proxy.send_overlay_input_async(input("git ")).await.unwrap();
        proxy.send_overlay_input_async(input("git ")).await.unwrap();
        proxy.send_overlay_input_async(input("git c")).await.unwrap();
        proxy
            .send_event_async(Event::GpuiOverlayEndInput {
                session_id: uuid::Uuid::nil(),
            })
            .await
            .unwrap();
        proxy.send_overlay_input_async(input("git c")).await.unwrap();
        proxy.send_overlay_input_async(input(" ")).await.unwrap();
        proxy.send_overlay_input_async(input(" ")).await.unwrap();
        assert_eq!(rx.len(), 6);
        assert_eq!(proxy.diagnostics().async_events, 6);
        for expected in [Some("git "), Some("git c"), None, Some("git c"), Some(" "), Some(" ")] {
            match (expected, rx.recv().unwrap()) {
                (Some(expected), Event::GpuiOverlayInput(queued)) => assert_eq!(queued.take(), Some(input(expected))),
                (None, Event::GpuiOverlayEndInput { .. }) => {},
                _ => panic!("input/control order changed"),
            }
        }
        assert_eq!(proxy.diagnostics().async_payload_bytes, 0);
    }

    #[tokio::test]
    async fn dropping_host_retires_queued_input_even_while_proxies_survive() {
        let (proxy, rx) = channel();
        let other_proxy = proxy.clone();
        proxy
            .send_overlay_input_async(input("git large-payload"))
            .await
            .unwrap();
        let payload = proxy.pending_input.lock().unwrap().clone();
        assert!(payload.upgrade().is_some());
        proxy.send_event(Event::ReloadAccessibility).unwrap();
        drop(rx);
        assert!(
            payload.upgrade().is_none(),
            "flume sender must not retain a disconnected queue"
        );
        assert_eq!(other_proxy.diagnostics().async_events, 0);
        assert_eq!(other_proxy.diagnostics().async_payload_bytes, 0);
        assert_eq!(other_proxy.diagnostics().sync_events, 0);
        assert_eq!(other_proxy.diagnostics().sync_payload_bytes, 0);
        assert_eq!(
            other_proxy.send_overlay_input_async(input("retry")).await,
            Err(EventAdmissionError::Closed)
        );
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
