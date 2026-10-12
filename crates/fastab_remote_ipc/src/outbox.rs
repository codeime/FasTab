//! One connection-owned queue for desktop commands, responses and heartbeats.
//!
//! Admission never waits for a socket. Queued and in-flight encoded frames share
//! one budget; retiring a connection also fails every outstanding RPC. A sender
//! clone belongs to exactly this connection and cannot revive after retirement.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use fastab_proto::FigProtobufEncodable;
use fastab_proto::figterm::{InsertTextRequest, InterceptRequest, SetBufferRequest, intercept_request};
use fastab_proto::prost::Message;
use fastab_proto::remote::clientbound::request::Request;
use fastab_proto::remote::{Clientbound, RunProcessRequest, clientbound, hostbound};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::{Notify, oneshot, watch};
use tokio::time::Instant;

use crate::figterm::FigtermCommand;

const MAX_PENDING_MESSAGES: usize = 256;
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
const MAX_PENDING_RESPONSES: usize = 256;
const FRAME_HEADER_BYTES: usize = 18;
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    Closed,
    CapacityExceeded,
    FrameTooLarge,
    EncodingFailed,
    Cancelled,
}

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "remote outbox: {self:?}")
    }
}

impl std::error::Error for SendError {}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct OutboxDiagnostics {
    pub closed: bool,
    /// Includes the frame currently being written.
    pub pending_messages: usize,
    pub pending_bytes: usize,
    pub pending_responses: usize,
}

struct PendingResponse {
    reply: oneshot::Sender<hostbound::response::Response>,
    deadline: Instant,
}

#[derive(Default)]
struct State {
    closed: bool,
    queue: VecDeque<Bytes>,
    pending_messages: usize,
    pending_bytes: usize,
    responses: HashMap<u64, PendingResponse>,
    nonce: u64,
}

struct Shared {
    state: Mutex<State>,
    closed: watch::Sender<bool>,
    queued: Notify,
    responses_changed: Notify,
}

impl Shared {
    fn close(&self, state: &mut State) {
        if state.closed {
            return;
        }
        state.closed = true;
        for bytes in state.queue.drain(..) {
            state.pending_messages -= 1;
            state.pending_bytes -= bytes.len();
        }
        state.responses.clear();
        self.closed.send_replace(true);
        self.queued.notify_one();
    }

    fn prune_responses(state: &mut State) {
        let now = Instant::now();
        state
            .responses
            .retain(|_, pending| !pending.reply.is_closed() && now < pending.deadline);
    }

    fn enqueue(&self, state: &mut State, message: Clientbound) -> Result<(), SendError> {
        if state.closed {
            return Err(SendError::Closed);
        }
        let size = message.encoded_len().checked_add(FRAME_HEADER_BYTES);
        let error = match size {
            None => Some(SendError::FrameTooLarge),
            Some(size) if size > MAX_PENDING_BYTES => Some(SendError::FrameTooLarge),
            Some(size)
                if state.pending_messages >= MAX_PENDING_MESSAGES || size > MAX_PENDING_BYTES - state.pending_bytes =>
            {
                Some(SendError::CapacityExceeded)
            },
            _ => None,
        };
        if let Some(error) = error {
            self.close(state);
            return Err(error);
        }
        // The length check precedes encoding. Retain only the encoded frame,
        // rather than both it and the caller's arbitrarily capacious strings.
        let bytes = match message.encode_fastab_protobuf() {
            Ok(bytes) => bytes,
            Err(_) => {
                self.close(state);
                return Err(SendError::EncodingFailed);
            },
        };
        state.pending_messages += 1;
        state.pending_bytes += bytes.len();
        state.queue.push_back(bytes);
        self.queued.notify_one();
        Ok(())
    }
}

/// A nonblocking submission handle. Success means admitted, not applied by the
/// terminal. Inserts are never silently dropped, merged, or replayed.
#[derive(Clone)]
pub struct FigtermSender {
    shared: Arc<Shared>,
}

impl fmt::Debug for FigtermSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.diagnostics().fmt(formatter)
    }
}

/// The sole receiver. Dropping it retires the connection, even if callers keep
/// sender clones. Exposed for embedders that transport the framed bytes.
pub struct Outbox {
    sender: FigtermSender,
}

/// RAII accounting keeps a popped frame charged until its write finishes or is
/// cancelled. There is no delivery retry after a partial write.
pub struct OutboundFrame {
    bytes: Bytes,
    sender: FigtermSender,
}

pub fn channel() -> (FigtermSender, Outbox) {
    let (closed, _) = watch::channel(false);
    let sender = FigtermSender {
        shared: Arc::new(Shared {
            state: Mutex::new(State::default()),
            closed,
            queued: Notify::new(),
            responses_changed: Notify::new(),
        }),
    };
    (sender.clone(), Outbox { sender })
}

impl FigtermSender {
    pub fn send(&self, command: FigtermCommand) -> Result<(), SendError> {
        let mut state = self.shared.state.lock();
        if state.closed {
            return Err(SendError::Closed);
        }
        Shared::prune_responses(&mut state);
        let mut pending = None;
        let request = match command {
            FigtermCommand::InterceptFigJs {
                intercept_keystrokes,
                intercept_global_keystrokes,
                actions,
                override_actions,
            } => Request::Intercept(InterceptRequest {
                intercept_command: Some(intercept_request::InterceptCommand::SetFigjsIntercepts(
                    intercept_request::SetFigjsIntercepts {
                        intercept_bound_keystrokes: intercept_keystrokes,
                        intercept_global_keystrokes,
                        actions,
                        override_actions,
                    },
                )),
            }),
            FigtermCommand::InterceptFigJSVisible { visible } => Request::Intercept(InterceptRequest {
                intercept_command: Some(intercept_request::InterceptCommand::SetFigjsVisible(
                    intercept_request::SetFigjsVisible { visible },
                )),
            }),
            FigtermCommand::InsertText {
                insertion,
                deletion,
                offset,
                immediate,
                insertion_buffer,
                insert_during_command,
            } => Request::InsertText(InsertTextRequest {
                insertion,
                deletion: deletion.map(|value| value as u64),
                offset,
                immediate,
                insertion_buffer,
                insert_during_command,
            }),
            FigtermCommand::SetBuffer { text, cursor_position } => {
                Request::SetBuffer(SetBufferRequest { text, cursor_position })
            },
            FigtermCommand::RunProcess {
                channel,
                executable,
                arguments,
                working_directory,
                env,
                timeout,
            } => {
                if channel.is_closed() {
                    return Err(SendError::Cancelled);
                }
                if state.responses.len() >= MAX_PENDING_RESPONSES || state.nonce == u64::MAX {
                    self.shared.close(&mut state);
                    return Err(SendError::CapacityExceeded);
                }
                let nonce = state.nonce;
                state.nonce += 1;
                let now = Instant::now();
                let deadline = now
                    .checked_add(timeout.unwrap_or(DEFAULT_RESPONSE_TIMEOUT))
                    .unwrap_or(now + DEFAULT_RESPONSE_TIMEOUT);
                pending = Some((
                    nonce,
                    PendingResponse {
                        reply: channel,
                        deadline,
                    },
                ));
                Request::RunProcess(RunProcessRequest {
                    executable,
                    arguments,
                    working_directory,
                    env,
                    timeout: timeout.map(Into::into),
                })
            },
        };
        self.shared.enqueue(
            &mut state,
            Clientbound {
                packet: Some(clientbound::Packet::Request(clientbound::Request {
                    request: Some(request),
                    nonce: pending.as_ref().map(|(nonce, _)| *nonce),
                })),
            },
        )?;
        if let Some((nonce, pending)) = pending {
            state.responses.insert(nonce, pending);
            self.shared.responses_changed.notify_one();
        }
        Ok(())
    }

    pub(crate) fn send_packet(&self, message: Clientbound) -> Result<(), SendError> {
        self.shared.enqueue(&mut self.shared.state.lock(), message)
    }

    pub(crate) fn respond(&self, nonce: u64, response: hostbound::response::Response) {
        let mut state = self.shared.state.lock();
        Shared::prune_responses(&mut state);
        if let Some(pending) = state.responses.remove(&nonce) {
            let _ = pending.reply.send(response);
        }
        self.shared.responses_changed.notify_one();
    }

    fn maintain(&self) -> Option<Instant> {
        let mut state = self.shared.state.lock();
        Shared::prune_responses(&mut state);
        state.responses.values().map(|pending| pending.deadline).min()
    }

    /// Maintain RPC ownership independently of the legacy heartbeat. With no
    /// outstanding replies this task waits on notifications, without a timer.
    pub(crate) async fn maintain_responses(&self) {
        loop {
            let changed = self.shared.responses_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = self.maintain();
            tokio::select! {
                biased;
                _ = self.closed() => return,
                _ = &mut changed => {},
                _ = poll_fn(|cx| {
                    let mut state = self.shared.state.lock();
                    // The oneshot registers this task for receiver cancellation.
                    // No extra watcher/task per pending RPC is retained.
                    if state.responses.values_mut().any(|pending| pending.reply.poll_closed(cx).is_ready()) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }) => {},
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {},
            }
        }
    }

    pub fn close(&self) {
        self.shared.close(&mut self.shared.state.lock());
    }

    pub async fn closed(&self) {
        let mut changes = self.shared.closed.subscribe();
        loop {
            if *changes.borrow_and_update() {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn diagnostics(&self) -> OutboxDiagnostics {
        let state = self.shared.state.lock();
        OutboxDiagnostics {
            closed: state.closed,
            pending_messages: state.pending_messages,
            pending_bytes: state.pending_bytes,
            pending_responses: state.responses.len(),
        }
    }
}

impl Outbox {
    pub fn try_recv(&mut self) -> Option<OutboundFrame> {
        self.sender
            .shared
            .state
            .lock()
            .queue
            .pop_front()
            .map(|bytes| OutboundFrame {
                bytes,
                sender: self.sender.clone(),
            })
    }

    pub async fn recv(&mut self) -> Option<OutboundFrame> {
        loop {
            let notified = self.sender.shared.queued.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.sender.shared.state.lock();
                if state.closed {
                    return None;
                }
                if let Some(bytes) = state.queue.pop_front() {
                    return Some(OutboundFrame {
                        bytes,
                        sender: self.sender.clone(),
                    });
                }
            }
            notified.await;
        }
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        self.sender.close();
    }
}

impl OutboundFrame {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for OutboundFrame {
    fn drop(&mut self) {
        let mut state = self.sender.shared.state.lock();
        state.pending_messages -= 1;
        state.pending_bytes -= self.bytes.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(size: usize) -> FigtermCommand {
        FigtermCommand::InsertText {
            insertion: Some("x".repeat(size)),
            deletion: None,
            offset: None,
            immediate: Some(false),
            insertion_buffer: None,
            insert_during_command: None,
        }
    }

    #[test]
    fn in_flight_frames_remain_charged_and_overflow_retires_the_connection() {
        let (sender, mut receiver) = channel();
        sender.send(insert(2 * 1024 * 1024)).unwrap();
        let frame = receiver.try_recv().unwrap();
        let pending = sender.diagnostics();
        assert_eq!(pending.pending_messages, 1);
        assert_eq!(pending.pending_bytes, frame.bytes().len());
        assert_eq!(sender.send(insert(2 * 1024 * 1024)), Err(SendError::CapacityExceeded));
        assert!(sender.diagnostics().closed);
        assert_eq!(sender.diagnostics().pending_messages, 1);
        drop(frame);
        assert_eq!(sender.diagnostics().pending_bytes, 0);
        assert_eq!(sender.diagnostics().pending_messages, 0);
        assert_eq!(sender.send(insert(1)), Err(SendError::Closed));
    }

    #[test]
    fn oversize_frames_close_without_encoding_or_leaving_response_waiters() {
        let (sender, _receiver) = channel();
        let (command, mut reply) = FigtermCommand::run_process("x".into(), vec![], None, vec![], None);
        sender.send(command).unwrap();
        assert_eq!(sender.diagnostics().pending_responses, 1);
        assert_eq!(sender.send(insert(MAX_PENDING_BYTES)), Err(SendError::FrameTooLarge));
        assert!(reply.try_recv().is_err());
        assert_eq!(sender.diagnostics().pending_responses, 0);
        assert_eq!(sender.diagnostics().pending_bytes, 0);
    }

    #[test]
    fn fifo_wire_frames_and_old_sender_remain_connection_scoped() {
        let (sender, mut receiver) = channel();
        sender
            .send(FigtermCommand::InterceptFigJSVisible { visible: true })
            .unwrap();
        sender.send(insert(3)).unwrap();
        sender
            .send(FigtermCommand::InterceptFigJSVisible { visible: false })
            .unwrap();
        let mut requests = Vec::new();
        while let Some(frame) = receiver.try_recv() {
            let (_, message) = fastab_proto::FigMessage::parse(&mut frame.bytes()).unwrap();
            let message = message.decode::<Clientbound>().unwrap();
            let Some(clientbound::Packet::Request(request)) = message.packet else {
                panic!("request")
            };
            requests.push(request.request.unwrap());
        }
        assert!(matches!(&requests[0], Request::Intercept(_)));
        assert!(matches!(&requests[1], Request::InsertText(insert) if insert.insertion.as_deref() == Some("xxx")));
        assert!(matches!(&requests[2], Request::Intercept(_)));
        drop(receiver);
        let (fresh, mut fresh_receiver) = channel();
        assert_eq!(sender.send(insert(1)), Err(SendError::Closed));
        fresh.send(insert(2)).unwrap();
        assert!(fresh_receiver.try_recv().is_some());
        assert_eq!(fresh.diagnostics().pending_messages, 0);
    }

    #[test]
    fn cancelled_or_expired_rpcs_release_the_response_budget() {
        let (sender, _receiver) = channel();
        let (command, reply) = FigtermCommand::run_process("x".into(), vec![], None, vec![], None);
        drop(reply);
        assert_eq!(sender.send(command), Err(SendError::Cancelled));
        assert_eq!(sender.diagnostics().pending_messages, 0);
        let (command, mut reply) = FigtermCommand::run_process("x".into(), vec![], None, vec![], Some(Duration::ZERO));
        sender.send(command).unwrap();
        assert!(sender.maintain().is_none());
        assert_eq!(sender.diagnostics().pending_responses, 0);
        assert!(reply.try_recv().is_err());
    }

    #[tokio::test]
    async fn maintenance_rearms_for_a_new_earlier_rpc_without_a_heartbeat() {
        use std::future::Future;

        let (sender, _receiver) = channel();
        let (long_command, mut long_reply) = FigtermCommand::run_process("long".into(), vec![], None, vec![], None);
        sender.send(long_command).unwrap();
        let maintenance = sender.maintain_responses();
        tokio::pin!(maintenance);
        // Park the real maintenance future on the long deadline before adding
        // a shorter RPC. No heartbeat task is present in this fixture.
        poll_fn(|cx| {
            assert!(maintenance.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let (short_command, short_reply) =
            FigtermCommand::run_process("short".into(), vec![], None, vec![], Some(Duration::from_millis(10)));
        sender.send(short_command).unwrap();
        let expired = async {
            tokio::select! {
                result = short_reply => assert!(result.is_err()),
                _ = &mut maintenance => panic!("maintenance exited before retirement"),
            }
        };
        tokio::time::timeout(Duration::from_secs(1), expired).await.unwrap();
        assert_eq!(sender.diagnostics().pending_responses, 1);
        assert!(matches!(
            long_reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        sender.respond(0, hostbound::response::Response::Error("fixture response".into()));
        assert!(matches!(long_reply.await, Ok(hostbound::response::Response::Error(_))));
        assert_eq!(sender.diagnostics().pending_responses, 0);
        sender.close();
        maintenance.await;
    }

    #[tokio::test]
    async fn receiver_cancellation_wakes_maintenance_and_releases_the_rpc() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};

        #[derive(Default)]
        struct WakeFlag(AtomicBool);

        impl Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Relaxed);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let (sender, _receiver) = channel();
        let (command, reply) = FigtermCommand::run_process("fixture".into(), vec![], None, vec![], None);
        sender.send(command).unwrap();
        let maintenance = sender.maintain_responses();
        tokio::pin!(maintenance);
        let wake = Arc::new(WakeFlag::default());
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        assert!(maintenance.as_mut().poll(&mut context).is_pending());
        wake.0.store(false, Ordering::Relaxed);
        drop(reply);
        assert!(wake.0.load(Ordering::Relaxed));
        assert!(maintenance.as_mut().poll(&mut context).is_pending());
        assert_eq!(sender.diagnostics().pending_responses, 0);
        sender.close();
        assert!(maintenance.as_mut().poll(&mut context).is_ready());
    }
}
