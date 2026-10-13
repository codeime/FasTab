use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use fastab_proto::FigProtobufEncodable;
use fastab_proto::prost::Message;
use fastab_proto::remote::Hostbound;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, watch};
use tracing::warn;

// Engineering limits, not measured optima. Count the frame being written too:
// a slow peer must not move an unbounded backlog out of the queue into writers.
const MAX_PENDING_MESSAGES: usize = 256;
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
const FRAME_HEADER_BYTES: usize = 18;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

const KEY_PENDING: u8 = 0;
const KEY_WRITTEN: u8 = 1;
const KEY_FAILED: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeginAttempt {
    /// A new connection generation may start.
    Started(Generation),
    /// An in-flight frame still owes its accounting debit. Retry later.
    Busy,
    /// The supervisor is finished (explicit stop or generation overflow).
    Stopped,
}

struct KeyDelivery {
    status: AtomicU8,
    changed: Arc<Notify>,
    // The EOF settlement barrier retires the generation owning a pending key.
    generation: Generation,
}

impl KeyDelivery {
    fn settle(&self, status: u8) {
        if self
            .status
            .compare_exchange(KEY_PENDING, status, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.changed.notify_one();
        }
    }
}

struct QueuedFrame {
    bytes: Bytes,
    key: Option<Arc<KeyDelivery>>,
}

impl Drop for QueuedFrame {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            key.settle(KEY_FAILED);
        }
    }
}

struct RecoverableKey {
    raw: Bytes,
    delivery: Arc<KeyDelivery>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Generation(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestOrigin {
    Local,
    Remote(Generation),
}

impl RequestOrigin {
    pub(crate) fn is_current(self, ready: Option<Generation>) -> bool {
        match self {
            Self::Local => true,
            Self::Remote(generation) => ready == Some(generation),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionPhase {
    Disconnected,
    Connecting(Generation),
    Handshaking(Generation),
    Ready(Generation),
    Stopped,
}

impl ConnectionPhase {
    pub(crate) fn ready_generation(self) -> Option<Generation> {
        match self {
            Self::Ready(generation) => Some(generation),
            _ => None,
        }
    }

    fn generation(self) -> Option<Generation> {
        match self {
            Self::Connecting(generation) | Self::Handshaking(generation) | Self::Ready(generation) => Some(generation),
            Self::Disconnected | Self::Stopped => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ContextProgress {
    pub(crate) full_context_admitted: bool,
    environment_epoch: Option<u64>,
}

impl ContextProgress {
    pub(crate) fn needs_environment(self, epoch: u64) -> bool {
        self.environment_epoch != Some(epoch)
    }

    pub(crate) fn record(&mut self, admitted: ContextAdmission) {
        self.full_context_admitted |= admitted.full_context;
        if let Some(epoch) = admitted.environment_epoch {
            self.environment_epoch = Some(epoch);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ContextAdmission {
    pub(crate) full_context: bool,
    pub(crate) environment_epoch: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    StaleGeneration,
    FrameTooLarge,
    CapacityExceeded,
    EncodingFailed,
}

struct State {
    phase: ConnectionPhase,
    last_generation: u64,
    queue: VecDeque<QueuedFrame>,
    pending_messages: usize,
    pending_bytes: usize,
    keys: VecDeque<RecoverableKey>,
    key_bytes: usize,
    context: ContextProgress,
}

struct Shared {
    state: Mutex<State>,
    changed: watch::Sender<ConnectionPhase>,
    queued: Notify,
    keys_changed: Arc<Notify>,
}

impl Shared {
    fn set_phase(&self, state: &mut State, phase: ConnectionPhase) {
        state.phase = phase;
        self.changed.send_replace(phase);
    }

    fn clear_queue(state: &mut State) {
        while let Some(frame) = state.queue.pop_front() {
            state.pending_messages -= 1;
            state.pending_bytes -= frame.bytes.len();
        }
        state.context = ContextProgress::default();
    }

    fn invalidate(&self, state: &mut State, generation: Generation, reason: &str) {
        if state.phase.generation() != Some(generation) {
            return;
        }
        warn!(?generation, reason, "Remote connection retired");
        Self::clear_queue(state);
        self.set_phase(state, ConnectionPhase::Disconnected);
        self.queued.notify_waiters();
    }
}

#[derive(Clone)]
pub(crate) struct RemoteSender {
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub(crate) struct GenerationSender {
    sender: RemoteSender,
    generation: Generation,
}

impl RemoteSender {
    pub(super) fn new() -> Self {
        let (changed, _) = watch::channel(ConnectionPhase::Disconnected);
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    phase: ConnectionPhase::Disconnected,
                    last_generation: 0,
                    queue: VecDeque::new(),
                    pending_messages: 0,
                    pending_bytes: 0,
                    keys: VecDeque::new(),
                    key_bytes: 0,
                    context: ContextProgress::default(),
                }),
                changed,
                queued: Notify::new(),
                keys_changed: Arc::new(Notify::new()),
            }),
        }
    }

    pub(crate) fn phase(&self) -> ConnectionPhase {
        self.shared.state.lock().unwrap().phase
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<ConnectionPhase> {
        self.shared.changed.subscribe()
    }

    pub(crate) fn for_generation(&self, generation: Generation) -> GenerationSender {
        GenerationSender {
            sender: self.clone(),
            generation,
        }
    }

    pub(crate) fn current(&self) -> Option<GenerationSender> {
        self.phase()
            .ready_generation()
            .map(|generation| self.for_generation(generation))
    }

    pub(super) fn begin_attempt(&self) -> BeginAttempt {
        let mut state = self.shared.state.lock().unwrap();
        if state.phase == ConnectionPhase::Stopped {
            return BeginAttempt::Stopped;
        }
        // Retiring clears queued frames, but an I/O future must release its
        // in-flight allocation before a new generation can receive a fresh budget.
        // Callers must retry on Busy — never treat it as a permanent stop.
        if state.pending_messages != 0 || state.pending_bytes != 0 {
            return BeginAttempt::Busy;
        }
        let Some(next) = state.last_generation.checked_add(1) else {
            self.shared.set_phase(&mut state, ConnectionPhase::Stopped);
            return BeginAttempt::Stopped;
        };
        let generation = Generation(next);
        state.last_generation = next;
        state.context = ContextProgress::default();
        self.shared
            .set_phase(&mut state, ConnectionPhase::Connecting(generation));
        BeginAttempt::Started(generation)
    }

    pub(super) fn set_handshaking(&self, generation: Generation) {
        let mut state = self.shared.state.lock().unwrap();
        if state.phase == ConnectionPhase::Connecting(generation) {
            self.shared
                .set_phase(&mut state, ConnectionPhase::Handshaking(generation));
        }
    }

    pub(super) fn set_ready(&self, generation: Generation) {
        let mut state = self.shared.state.lock().unwrap();
        if state.phase == ConnectionPhase::Handshaking(generation) {
            self.shared.set_phase(&mut state, ConnectionPhase::Ready(generation));
        }
    }

    pub(super) fn retire(&self, generation: Generation, reason: &str) {
        let mut state = self.shared.state.lock().unwrap();
        self.shared.invalidate(&mut state, generation, reason);
    }

    pub(super) fn stop(&self) {
        let mut state = self.shared.state.lock().unwrap();
        Shared::clear_queue(&mut state);
        self.shared.set_phase(&mut state, ConnectionPhase::Stopped);
        self.shared.queued.notify_waiters();
    }

    pub(super) async fn invalidated(&self, generation: Generation) {
        let mut changes = self.subscribe();
        loop {
            if self.phase().generation() != Some(generation) {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) async fn next_frame(&self, generation: Generation) -> Option<PendingFrame> {
        loop {
            let notified = self.shared.queued.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.shared.state.lock().unwrap();
                if state.phase != ConnectionPhase::Ready(generation) {
                    return None;
                }
                if let Some(frame) = state.queue.pop_front() {
                    return Some(PendingFrame {
                        frame,
                        sender: self.clone(),
                    });
                }
            }
            notified.await;
        }
    }

    /// True while at least one intercepted key is still awaiting write settlement.
    /// Ordinary input must not overtake those keys, but the PTY main loop must
    /// not block waiting for them either.
    pub(crate) fn has_pending_keys(&self) -> bool {
        let state = self.shared.state.lock().unwrap();
        state
            .keys
            .iter()
            .any(|key| key.delivery.status.load(Ordering::Acquire) == KEY_PENDING)
    }

    pub(crate) async fn key_delivery_changed(&self) {
        self.shared.keys_changed.notified().await;
    }

    /// The PTY main loop is the sole consumer. Keep key order even when queued
    /// keys fail before the writer drops the earlier in-flight key.
    pub(crate) fn take_failed_keys(&self) -> Vec<Bytes> {
        let mut state = self.shared.state.lock().unwrap();
        let mut failed = Vec::new();
        while let Some(key) = state.keys.front() {
            let status = key.delivery.status.load(Ordering::Acquire);
            if status == KEY_PENDING {
                break;
            }
            let key = state.keys.pop_front().unwrap();
            state.key_bytes -= key.raw.len();
            if status == KEY_FAILED {
                failed.push(key.raw);
            }
        }
        failed
    }

    /// Preserve the position of ordinary/rejected input after older intercepted
    /// keys when input ends. Ordinary input uses the non-blocking
    /// [`take_failed_keys`] + [`has_pending_keys`] path instead.
    pub(crate) async fn take_failed_keys_before_input(&self) -> Vec<Bytes> {
        self.take_failed_keys_with_timeout(WRITE_TIMEOUT).await
    }

    async fn take_failed_keys_with_timeout(&self, limit: Duration) -> Vec<Bytes> {
        let mut failed = Vec::new();
        let deadline = tokio::time::sleep(limit);
        tokio::pin!(deadline);
        let mut retired = false;
        loop {
            let changed = self.shared.keys_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            failed.extend(self.take_failed_keys());
            if self.shared.state.lock().unwrap().keys.is_empty() {
                break;
            }
            tokio::select! {
                biased;
                _ = &mut deadline, if !retired => {
                    // One deadline for the entire barrier, not 256 successive
                    // per-frame budgets. Retirement cancels the supervisor's
                    // scoped writer. Only its actual drop may fail an in-flight
                    // key; never guess delivery or replay before cancellation.
                    retired = true;
                    let mut state = self.shared.state.lock().unwrap();
                    let generation = state.keys.iter()
                        .find(|key| key.delivery.status.load(Ordering::Acquire) == KEY_PENDING)
                        .map(|key| key.delivery.generation);
                    if let Some(generation) = generation {
                        self.shared.invalidate(&mut state, generation, "intercepted input settlement timed out");
                    }
                }
                _ = &mut changed => {}
            }
        }
        failed
    }
}

#[cfg(test)]
mod test_support {
    use super::*;

    // Keep private outbox state and frame internals inside their owner while
    // exercising the production EOF consumer from main.rs.
    pub(crate) struct HeldFrame {
        _frame: PendingFrame,
    }

    impl HeldFrame {
        pub(crate) async fn write_to(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
            self._frame.write_to(writer).await
        }
    }

    impl RemoteSender {
        pub(crate) fn test_disconnected() -> Self {
            Self::new()
        }

        pub(crate) fn test_ready() -> Self {
            let sender = Self::new();
            let BeginAttempt::Started(generation) = sender.begin_attempt() else {
                panic!("fresh sender must start a generation");
            };
            sender.set_handshaking(generation);
            sender.set_ready(generation);
            sender
        }

        pub(crate) async fn test_hold_frame(&self) -> HeldFrame {
            let generation = self.phase().ready_generation().unwrap();
            HeldFrame {
                _frame: self.next_frame(generation).await.unwrap(),
            }
        }

        pub(crate) fn test_retire(&self) {
            self.retire(self.phase().ready_generation().unwrap(), "EOF test retirement");
        }
    }
}

impl GenerationSender {
    pub(crate) fn generation(&self) -> Generation {
        self.generation
    }

    pub(crate) fn is_current_ready(&self) -> bool {
        self.sender.phase() == ConnectionPhase::Ready(self.generation)
    }

    pub(crate) async fn invalidated(&self) {
        self.sender.invalidated(self.generation).await;
    }

    pub(crate) fn context_progress(&self) -> Option<ContextProgress> {
        let state = self.sender.shared.state.lock().unwrap();
        (state.phase == ConnectionPhase::Ready(self.generation)).then_some(state.context)
    }

    pub(crate) fn try_send(&self, message: Hostbound) -> Result<(), AdmissionError> {
        self.try_send_with_context(message, ContextAdmission::default())
    }

    pub(crate) fn try_send_with_context(
        &self,
        message: Hostbound,
        context: ContextAdmission,
    ) -> Result<(), AdmissionError> {
        self.admit(message, context, None)
    }

    /// Raw input is recoverable until write_all succeeds, not merely until
    /// admission. InterceptedKey never commits context synchronization metadata.
    pub(crate) fn try_send_key(&self, message: Hostbound, raw: Bytes) -> Result<(), AdmissionError> {
        self.admit(message, ContextAdmission::default(), Some(raw))
    }

    fn admit(&self, message: Hostbound, context: ContextAdmission, raw: Option<Bytes>) -> Result<(), AdmissionError> {
        if !self.is_current_ready() {
            return Err(AdmissionError::StaleGeneration);
        }
        let frame_len = message.encoded_len().checked_add(FRAME_HEADER_BYTES);
        if frame_len.is_none_or(|len| len > MAX_PENDING_BYTES) {
            return Err(AdmissionError::FrameTooLarge);
        }
        let encoded = match message.encode_fastab_protobuf() {
            Ok(encoded) => encoded,
            Err(err) => {
                self.sender
                    .retire(self.generation, &format!("outgoing frame encoding failed: {err}"));
                return Err(AdmissionError::EncodingFailed);
            },
        };
        let mut state = self.sender.shared.state.lock().unwrap();
        if state.phase != ConnectionPhase::Ready(self.generation) {
            return Err(AdmissionError::StaleGeneration);
        }
        // Completed keys need no recovery allocation. In particular a batch of
        // successfully written keys must not fill the fallback-record budget.
        while state
            .keys
            .front()
            .is_some_and(|key| key.delivery.status.load(Ordering::Acquire) == KEY_WRITTEN)
        {
            let key = state.keys.pop_front().unwrap();
            state.key_bytes -= key.raw.len();
        }
        if state.pending_messages >= MAX_PENDING_MESSAGES
            || encoded.len() > MAX_PENDING_BYTES - state.pending_bytes
            || raw.as_ref().is_some_and(|raw| {
                state.keys.len() >= MAX_PENDING_MESSAGES || raw.len() > MAX_PENDING_BYTES - state.key_bytes
            })
        {
            self.sender
                .shared
                .invalidate(&mut state, self.generation, "outgoing queue exceeds budget");
            return Err(AdmissionError::CapacityExceeded);
        }
        state.pending_messages += 1;
        state.pending_bytes += encoded.len();
        let key = raw.map(|raw| {
            let delivery = Arc::new(KeyDelivery {
                status: AtomicU8::new(KEY_PENDING),
                changed: self.sender.shared.keys_changed.clone(),
                generation: self.generation,
            });
            state.key_bytes += raw.len();
            state.keys.push_back(RecoverableKey {
                raw,
                delivery: delivery.clone(),
            });
            delivery
        });
        state.queue.push_back(QueuedFrame { bytes: encoded, key });
        state.context.record(context);
        drop(state);
        self.sender.shared.queued.notify_one();
        Ok(())
    }
}

pub(super) struct PendingFrame {
    frame: QueuedFrame,
    sender: RemoteSender,
}

impl PendingFrame {
    pub(super) async fn write_to(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        self.write_with_timeout(writer, WRITE_TIMEOUT).await
    }

    async fn write_with_timeout(&mut self, writer: &mut (impl AsyncWrite + Unpin), limit: Duration) -> io::Result<()> {
        tokio::time::timeout(limit, async {
            writer.write_all(&self.frame.bytes).await?;
            // Flush failure is not evidence that a fully written key was lost.
            // Never replay it after crossing the explicit write_all boundary.
            if let Some(key) = &self.frame.key {
                key.settle(KEY_WRITTEN);
            }
            writer.flush().await
        })
        .await
        .map_err(|_elapsed| io::Error::new(io::ErrorKind::TimedOut, "remote frame write timed out"))?
    }
}

impl Drop for PendingFrame {
    fn drop(&mut self) {
        let mut state = self.sender.shared.state.lock().unwrap();
        // The counters describe actual outstanding frames, across phase changes.
        // begin_attempt cannot replace the generation until this debit finishes.
        state.pending_messages -= 1;
        state.pending_bytes -= self.frame.bytes.len();
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};

    use fastab_proto::remote_hooks::{hook_to_message, new_intercepted_key_hook};

    use super::*;

    fn message(size: usize) -> Hostbound {
        hook_to_message(new_intercepted_key_hook(None, "navigateDown", "x".repeat(size)))
    }

    fn ready() -> (RemoteSender, GenerationSender) {
        let sender = RemoteSender::new();
        let BeginAttempt::Started(generation) = sender.begin_attempt() else {
            panic!("expected a fresh generation");
        };
        sender.set_handshaking(generation);
        sender.set_ready(generation);
        let bound = sender.for_generation(generation);
        (sender, bound)
    }

    /// A real AsyncWrite boundary: accepts a prefix then stalls, or finishes
    /// writing and fails/stalls only during flush. No desktop/PTY is opened.
    struct Writer {
        bytes: Vec<u8>,
        limit: usize,
        fail_flush: bool,
        stall_flush: bool,
        started: Option<Arc<Notify>>,
    }

    impl Writer {
        fn new(limit: usize) -> Self {
            Self {
                bytes: Vec::new(),
                limit,
                fail_flush: false,
                stall_flush: false,
                started: None,
            }
        }
    }

    impl AsyncWrite for Writer {
        fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
            let count = bytes.len().min(self.limit.saturating_sub(self.bytes.len()));
            if count == 0 {
                return Poll::Pending;
            }
            self.bytes.extend_from_slice(&bytes[..count]);
            if let Some(started) = &self.started {
                started.notify_one();
            }
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.stall_flush {
                Poll::Pending
            } else if self.fail_flush {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn oversized_frame_preserves_ready_queue_and_context() {
        let (sender, bound) = ready();
        bound
            .try_send_with_context(
                message(1),
                ContextAdmission {
                    full_context: true,
                    environment_epoch: Some(7),
                },
            )
            .unwrap();
        let before = {
            let state = sender.shared.state.lock().unwrap();
            (
                state.phase,
                state.pending_bytes,
                state.queue.front().unwrap().bytes.clone(),
            )
        };
        assert_eq!(
            bound.try_send_with_context(
                message(MAX_PENDING_BYTES),
                ContextAdmission {
                    full_context: true,
                    environment_epoch: Some(8),
                }
            ),
            Err(AdmissionError::FrameTooLarge)
        );
        let state = sender.shared.state.lock().unwrap();
        assert_eq!(state.phase, before.0);
        assert_eq!(state.pending_messages, 1);
        assert_eq!(state.pending_bytes, before.1);
        assert_eq!(state.queue.front().unwrap().bytes, before.2);
        assert!(state.context.full_context_admitted);
        assert!(!state.context.needs_environment(7));
        assert!(state.context.needs_environment(8));
    }

    #[test]
    fn cumulative_bytes_retire_and_clear_the_generation() {
        let (sender, bound) = ready();
        bound
            .try_send_with_context(
                message(MAX_PENDING_BYTES / 2),
                ContextAdmission {
                    full_context: true,
                    environment_epoch: Some(7),
                },
            )
            .unwrap();
        assert_eq!(
            bound.try_send(message(MAX_PENDING_BYTES / 2)),
            Err(AdmissionError::CapacityExceeded)
        );
        let state = sender.shared.state.lock().unwrap();
        assert_eq!(state.phase, ConnectionPhase::Disconnected);
        assert!(state.queue.is_empty());
        assert_eq!((state.pending_messages, state.pending_bytes), (0, 0));
        assert!(!state.context.full_context_admitted);
        assert!(state.context.needs_environment(7));
    }

    #[tokio::test]
    async fn count_budget_includes_inflight_and_release_precedes_next_generation() {
        let (sender, bound) = ready();
        bound.try_send(message(0)).unwrap();
        let frame = sender.next_frame(bound.generation()).await.unwrap();
        for _ in 1..MAX_PENDING_MESSAGES {
            bound.try_send(message(0)).unwrap();
        }
        assert_eq!(bound.try_send(message(0)), Err(AdmissionError::CapacityExceeded));
        assert_eq!(sender.shared.state.lock().unwrap().pending_messages, 1);
        assert_eq!(sender.begin_attempt(), BeginAttempt::Busy);
        drop(frame);
        {
            let state = sender.shared.state.lock().unwrap();
            assert_eq!((state.pending_messages, state.pending_bytes), (0, 0));
        }
        let BeginAttempt::Started(next) = sender.begin_attempt() else {
            panic!("expected the next generation after the in-flight frame released");
        };
        assert_ne!(next, bound.generation());
        sender.set_handshaking(next);
        sender.set_ready(next);
        assert_eq!(bound.try_send(message(0)), Err(AdmissionError::StaleGeneration));
        let next_sender = sender.for_generation(next);
        assert!(!next_sender.context_progress().unwrap().full_context_admitted);
        next_sender.try_send(message(0)).unwrap();
        drop(sender.next_frame(next).await.unwrap());
        assert_eq!(sender.shared.state.lock().unwrap().pending_bytes, 0);
    }

    #[tokio::test]
    async fn oversized_key_is_not_registered_for_a_second_fallback() {
        let (sender, bound) = ready();
        let raw = Bytes::from_static(b"\x1b[A");
        assert_eq!(
            bound.try_send_key(message(MAX_PENDING_BYTES), raw.clone()),
            Err(AdmissionError::FrameTooLarge)
        );
        // The caller still owns the exact original bytes for its immediate
        // fallback. The outbox must never produce a second copy on retirement.
        assert_eq!(raw.as_ref(), b"\x1b[A");
        assert!(sender.take_failed_keys_before_input().await.is_empty());
        assert_eq!(sender.phase(), ConnectionPhase::Ready(bound.generation()));
        sender.retire(bound.generation(), "test");
        assert!(sender.take_failed_keys().is_empty());
    }

    #[tokio::test]
    async fn rejected_key_waits_behind_inflight_and_queued_failed_keys() {
        let (sender, bound) = ready();
        // Keep A in-flight and B queued, then fill the remaining message slots.
        bound.try_send_key(message(0), Bytes::from_static(b"A")).unwrap();
        let inflight = sender.next_frame(bound.generation()).await.unwrap();
        bound.try_send_key(message(0), Bytes::from_static(b"B")).unwrap();
        for _ in 2..MAX_PENDING_MESSAGES {
            bound.try_send(message(0)).unwrap();
        }
        let rejected = Bytes::from_static(b"C");
        assert_eq!(
            bound.try_send_key(message(0), rejected.clone()),
            Err(AdmissionError::CapacityExceeded)
        );
        assert!(sender.take_failed_keys().is_empty());
        let barrier = sender.take_failed_keys_before_input();
        tokio::pin!(barrier);
        std::future::poll_fn(|cx| {
            assert!(barrier.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(inflight);
        let recovered = barrier.await;
        let mut pty = recovered.concat();
        pty.extend_from_slice(&rejected);
        assert_eq!(pty, b"ABC");
        assert!(sender.take_failed_keys_before_input().await.is_empty());
    }

    #[tokio::test]
    async fn ordinary_input_waits_for_partial_write_timeout_and_preserves_raw_bytes() {
        let (sender, bound) = ready();
        let raw = Bytes::from_static(b"\xff\x1b[A");
        bound.try_send_key(message(8), raw.clone()).unwrap();
        let mut frame = sender.next_frame(bound.generation()).await.unwrap();
        let mut writer = Writer::new(7);
        let error = frame
            .write_with_timeout(&mut writer, Duration::from_millis(1))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(writer.bytes.len(), 7);
        sender.retire(bound.generation(), "timed out");
        let barrier = sender.take_failed_keys_before_input();
        tokio::pin!(barrier);
        std::future::poll_fn(|cx| {
            assert!(barrier.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(frame);
        let mut pty = barrier.await.concat();
        pty.extend_from_slice(b"ordinary input");
        assert_eq!(&pty[..raw.len()], raw.as_ref());
        assert_eq!(&pty[raw.len()..], b"ordinary input");
        assert!(sender.take_failed_keys().is_empty());
    }

    #[tokio::test]
    async fn full_write_is_not_replayed_when_flush_fails() {
        for fail_flush in [false, true] {
            let (sender, bound) = ready();
            bound.try_send_key(message(8), Bytes::from_static(b"A")).unwrap();
            let context = bound.context_progress().unwrap();
            assert!(!context.full_context_admitted);
            assert!(context.needs_environment(0));
            let mut frame = sender.next_frame(bound.generation()).await.unwrap();
            let expected = frame.frame.bytes.clone();
            let mut writer = Writer::new(usize::MAX);
            writer.fail_flush = fail_flush;
            let result = frame.write_to(&mut writer).await;
            if fail_flush {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
            } else {
                result.unwrap();
            }
            assert_eq!(writer.bytes, expected.as_ref());
            sender.retire(bound.generation(), "connection closed");
            drop(frame);
            assert!(sender.take_failed_keys_before_input().await.is_empty());
        }
    }

    #[tokio::test]
    async fn cancel_during_flush_does_not_replay_a_fully_written_key() {
        let (sender, bound) = ready();
        bound.try_send_key(message(8), Bytes::from_static(b"A")).unwrap();
        let mut frame = sender.next_frame(bound.generation()).await.unwrap();
        let mut writer = Writer::new(usize::MAX);
        writer.stall_flush = true;
        {
            let write = frame.write_to(&mut writer);
            tokio::pin!(write);
            std::future::poll_fn(|cx| {
                assert!(write.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        assert!(!writer.bytes.is_empty());
        sender.stop();
        drop(frame);
        assert!(sender.take_failed_keys_before_input().await.is_empty());
        assert_eq!(sender.shared.state.lock().unwrap().pending_messages, 0);
    }

    #[tokio::test]
    async fn pending_keys_block_ordinary_flush_until_settled() {
        let (sender, bound) = ready();
        bound.try_send_key(message(0), Bytes::from_static(b"A")).unwrap();
        assert!(sender.has_pending_keys());
        assert!(sender.take_failed_keys().is_empty());
        let frame = sender.next_frame(bound.generation()).await.unwrap();
        assert!(sender.has_pending_keys());
        drop(frame);
        assert!(!sender.has_pending_keys());
        assert_eq!(sender.take_failed_keys(), vec![Bytes::from_static(b"A")]);
        assert!(!sender.has_pending_keys());
    }

    #[tokio::test]
    async fn undrained_raw_recovery_has_its_own_byte_bound() {
        let (sender, bound) = ready();
        let raw = Bytes::from(vec![b'a'; MAX_PENDING_BYTES]);
        bound.try_send_key(message(0), raw).unwrap();
        assert_eq!(
            bound.try_send_key(message(0), Bytes::from_static(b"b")),
            Err(AdmissionError::CapacityExceeded)
        );
        let failed = sender.take_failed_keys_before_input().await;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].len(), MAX_PENDING_BYTES);
        assert_eq!(sender.shared.state.lock().unwrap().key_bytes, 0);
    }

    #[tokio::test]
    async fn barrier_deadline_cancels_scoped_partial_writer_before_replaying() {
        let (sender, bound) = ready();
        let generation = bound.generation();
        bound.try_send_key(message(8), Bytes::from_static(b"A")).unwrap();
        bound.try_send_key(message(8), Bytes::from_static(b"B")).unwrap();
        let started = Arc::new(Notify::new());
        let mut writer = Writer::new(7);
        writer.started = Some(started.clone());
        let supervisor = sender.clone();
        let worker = tokio::spawn(async move {
            {
                let send = async {
                    while let Some(mut frame) = supervisor.next_frame(generation).await {
                        frame.write_to(&mut writer).await.unwrap();
                    }
                };
                tokio::select! {
                    _ = supervisor.invalidated(generation) => {},
                    _ = send => panic!("partial writer should be cancelled by the barrier"),
                }
            }
            writer
        });
        started.notified().await;
        let failed = sender.take_failed_keys_with_timeout(Duration::from_millis(1)).await;
        let writer = worker.await.unwrap();
        assert_eq!(writer.bytes.len(), 7);
        assert_eq!(failed.concat(), b"AB");
        assert_eq!(sender.phase(), ConnectionPhase::Disconnected);
        assert_eq!(sender.shared.state.lock().unwrap().pending_messages, 0);
        assert!(sender.take_failed_keys().is_empty());
    }

    struct SlowFlushWriter {
        delay: Pin<Box<tokio::time::Sleep>>,
        delivered: Arc<AtomicUsize>,
        started: Arc<Notify>,
    }

    impl AsyncWrite for SlowFlushWriter {
        fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
            self.delivered.fetch_add(1, Ordering::SeqCst);
            self.delay
                .as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_millis(2));
            self.started.notify_one();
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.delay.as_mut().poll(cx).map(|()| Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn barrier_has_one_deadline_despite_successive_completed_frames() {
        let (sender, bound) = ready();
        let generation = bound.generation();
        for key in 0..64 {
            bound.try_send_key(message(0), Bytes::from(vec![key])).unwrap();
        }
        let delivered = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let mut writer = SlowFlushWriter {
            delay: Box::pin(tokio::time::sleep(Duration::ZERO)),
            delivered: delivered.clone(),
            started: started.clone(),
        };
        let supervisor = sender.clone();
        let worker = tokio::spawn(async move {
            let send = async {
                while let Some(mut frame) = supervisor.next_frame(generation).await {
                    frame.write_to(&mut writer).await.unwrap();
                }
            };
            tokio::select! {
                _ = supervisor.invalidated(generation) => {},
                _ = send => {},
            }
        });
        started.notified().await;
        let failed = sender.take_failed_keys_with_timeout(Duration::from_millis(15)).await;
        worker.await.unwrap();
        let delivered = delivered.load(Ordering::SeqCst);
        assert!((1..64).contains(&delivered));
        assert_eq!(failed.len(), 64 - delivered);
        assert_eq!(failed.concat(), (delivered as u8..64).collect::<Vec<_>>());
        assert_eq!(sender.phase(), ConnectionPhase::Disconnected);
        assert_eq!(sender.shared.state.lock().unwrap().pending_messages, 0);
    }
}
