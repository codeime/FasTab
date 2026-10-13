//! Admission lives until main-loop handling, reply delivery, or RunProcess exit.
//! Decoders waiting for admission are separately observable and connection-bounded.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::{Result, bail};
use fastab_proto::fig::EnvironmentVariable;
use fastab_proto::figterm::{
    FigtermRequestMessage, FigtermResponseMessage, InterceptRequest, figterm_request_message, figterm_response_message,
    intercept_request,
};
use fastab_proto::prost::Message;
use fastab_proto::remote::{Clientbound, clientbound};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) const MAX_MESSAGES: usize = 256;
const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_CONNECTIONS: usize = 8;
const MAX_PROCESSES: usize = 8;
static OPEN_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

static BUDGET: LazyLock<Arc<IncomingBudget>> = LazyLock::new(|| IncomingBudget::new(MAX_MESSAGES, MAX_BYTES));
pub(crate) static CONNECTIONS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONNECTIONS)));
pub(crate) static PROCESS_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(MAX_PROCESSES)));

pub(crate) fn budget() -> Arc<IncomingBudget> {
    BUDGET.clone()
}

pub(crate) struct ConnectionLease {
    _permit: OwnedSemaphorePermit,
}

impl ConnectionLease {
    pub(crate) fn new(permit: OwnedSemaphorePermit) -> Self {
        OPEN_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        Self { _permit: permit }
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        OPEN_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct IncomingBudget {
    messages: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    max_bytes: usize,
    admitted_messages: AtomicUsize,
    admitted_bytes: AtomicUsize,
    waiting_messages: AtomicUsize,
    waiting_bytes: AtomicUsize,
}

impl IncomingBudget {
    pub(crate) fn new(messages: usize, bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            messages: Arc::new(Semaphore::new(messages)),
            bytes: Arc::new(Semaphore::new(bytes)),
            max_bytes: bytes,
            admitted_messages: AtomicUsize::new(0),
            admitted_bytes: AtomicUsize::new(0),
            waiting_messages: AtomicUsize::new(0),
            waiting_bytes: AtomicUsize::new(0),
        })
    }

    pub(crate) async fn admit(self: &Arc<Self>, bytes: usize) -> Result<IncomingPermit> {
        if bytes > self.max_bytes {
            bail!("decoded incoming payload exceeds its byte budget");
        }
        let waiting = WaitingAdmission::new(self.clone(), bytes);
        let count = self.messages.clone().acquire_owned().await?;
        let allocation = self.bytes.clone().acquire_many_owned(u32::try_from(bytes)?).await?;
        self.admitted_messages.fetch_add(1, Ordering::Relaxed);
        self.admitted_bytes.fetch_add(bytes, Ordering::Relaxed);
        drop(waiting);
        Ok(IncomingPermit {
            _count: count,
            _allocation: allocation,
            owner: self.clone(),
            bytes,
        })
    }
}

pub(crate) struct IncomingPermit {
    _count: OwnedSemaphorePermit,
    _allocation: OwnedSemaphorePermit,
    owner: Arc<IncomingBudget>,
    bytes: usize,
}

impl IncomingPermit {
    pub(crate) fn try_extend(&mut self, bytes: usize) -> bool {
        let Ok(bytes_u32) = u32::try_from(bytes) else {
            return false;
        };
        let Ok(extra) = self.owner.bytes.clone().try_acquire_many_owned(bytes_u32) else {
            return false;
        };
        self._allocation.merge(extra);
        self.bytes += bytes;
        self.owner.admitted_bytes.fetch_add(bytes, Ordering::Relaxed);
        true
    }
}

impl Drop for IncomingPermit {
    fn drop(&mut self) {
        self.owner.admitted_messages.fetch_sub(1, Ordering::Relaxed);
        self.owner.admitted_bytes.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

struct WaitingAdmission {
    owner: Arc<IncomingBudget>,
    bytes: usize,
}

impl WaitingAdmission {
    fn new(owner: Arc<IncomingBudget>, bytes: usize) -> Self {
        owner.waiting_messages.fetch_add(1, Ordering::Relaxed);
        owner.waiting_bytes.fetch_add(bytes, Ordering::Relaxed);
        Self { owner, bytes }
    }
}

impl Drop for WaitingAdmission {
    fn drop(&mut self) {
        self.owner.waiting_messages.fetch_sub(1, Ordering::Relaxed);
        self.owner.waiting_bytes.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

fn extra_string(value: &str, capacity: usize) -> usize {
    capacity.saturating_sub(value.len())
}

fn extra_optional(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, |value| extra_string(value, value.capacity()))
}

fn environment_overhead(values: &Vec<EnvironmentVariable>) -> usize {
    values.capacity() * size_of::<EnvironmentVariable>()
        + values
            .iter()
            .map(|value| extra_string(&value.key, value.key.capacity()) + extra_optional(&value.value))
            .sum::<usize>()
}

pub(crate) fn environment_size(values: &Vec<EnvironmentVariable>) -> usize {
    size_of::<Vec<EnvironmentVariable>>()
        + values.capacity() * size_of::<EnvironmentVariable>()
        + values
            .iter()
            .map(|value| value.key.capacity() + value.value.as_ref().map_or(0, String::capacity))
            .sum::<usize>()
}

fn intercept_overhead(request: &InterceptRequest) -> usize {
    match &request.intercept_command {
        Some(intercept_request::InterceptCommand::SetFigjsIntercepts(request)) => {
            request.actions.capacity() * size_of::<fastab_proto::figterm::Action>()
                + request
                    .actions
                    .iter()
                    .map(|action| {
                        extra_string(&action.identifier, action.identifier.capacity())
                            + action.bindings.capacity() * size_of::<String>()
                            + action
                                .bindings
                                .iter()
                                .map(|value| extra_string(value, value.capacity()))
                                .sum::<usize>()
                    })
                    .sum::<usize>()
        },
        _ => 0,
    }
}

pub(crate) fn local_size(message: &FigtermRequestMessage) -> usize {
    use figterm_request_message::Request;
    let extra = match &message.request {
        Some(Request::Intercept(request)) => intercept_overhead(request),
        Some(Request::InsertText(request)) => {
            extra_optional(&request.insertion) + extra_optional(&request.insertion_buffer)
        },
        Some(Request::SetBuffer(request)) => extra_string(&request.text, request.text.capacity()),
        Some(Request::InsertOnNewCmd(request)) => extra_string(&request.text, request.text.capacity()),
        Some(Request::UpdateShellContext(request)) => {
            environment_overhead(&request.environment_variables) + extra_optional(&request.alias)
        },
        Some(Request::NotifySshSessionStarted(request)) => {
            extra_string(&request.uuid, request.uuid.capacity())
                + extra_string(&request.remote_host, request.remote_host.capacity())
        },
        Some(Request::InlineShellCompletion(request)) => extra_string(&request.buffer, request.buffer.capacity()),
        Some(Request::InlineShellCompletionAccept(request)) => {
            extra_string(&request.buffer, request.buffer.capacity())
                + extra_string(&request.suggestion, request.suggestion.capacity())
        },
        Some(Request::Telemtety(request)) => extra_string(&request.event_blob, request.event_blob.capacity()),
        _ => 0,
    };
    // Encoded bytes cover string contents; add actual vector storage, spare
    // string capacity, and envelope/oneshot overhead. Allocator headers remain estimates.
    message.encoded_len() + size_of::<FigtermRequestMessage>() + 128 + extra
}

pub(crate) fn remote_size(message: &Clientbound) -> usize {
    use clientbound::request::Request;
    let extra = match &message.packet {
        Some(clientbound::Packet::Request(packet)) => match &packet.request {
            Some(Request::Intercept(request)) => intercept_overhead(request),
            Some(Request::InsertText(request)) => {
                extra_optional(&request.insertion) + extra_optional(&request.insertion_buffer)
            },
            Some(Request::SetBuffer(request)) => extra_string(&request.text, request.text.capacity()),
            Some(Request::InsertOnNewCmd(request)) => extra_string(&request.text, request.text.capacity()),
            Some(Request::RunProcess(request)) => {
                extra_string(&request.executable, request.executable.capacity())
                    + extra_optional(&request.working_directory)
                    + request.arguments.capacity() * size_of::<String>()
                    + request
                        .arguments
                        .iter()
                        .map(|value| extra_string(value, value.capacity()))
                        .sum::<usize>()
                    + environment_overhead(&request.env)
            },
            _ => 0,
        },
        Some(clientbound::Packet::NotifyChildSessionStarted(request)) => {
            extra_string(&request.parent_id, request.parent_id.capacity())
        },
        _ => 0,
    };
    message.encoded_len() + size_of::<Clientbound>() + 128 + extra
}

pub(crate) fn response_size(message: &FigtermResponseMessage) -> usize {
    let context = match &message.response {
        Some(figterm_response_message::Response::Diagnostics(response)) => response.shell_context.as_ref(),
        _ => None,
    };
    let extra = context.map_or(0, |context| {
        size_of::<fastab_proto::local::ShellContext>() + environment_overhead(&context.environment_variables)
    });
    // Charge the owned reply plus prost body/encoded frame temporarily held
    // together by encode_fastab_protobuf. This remains an allocation estimate.
    message.encoded_len() * 3 + size_of::<FigtermResponseMessage>() + 128 + extra
}

pub(crate) fn diagnostics_json() -> String {
    format!(
        "{{\"messages\":{},\"estimated_bytes\":{},\"waiting_decoded_messages\":{},\"waiting_decoded_estimated_bytes\":{},\"connections\":{},\"processes\":{},\"message_limit\":{},\"estimated_byte_limit\":{}}}",
        BUDGET.admitted_messages.load(Ordering::Relaxed),
        BUDGET.admitted_bytes.load(Ordering::Relaxed),
        BUDGET.waiting_messages.load(Ordering::Relaxed),
        BUDGET.waiting_bytes.load(Ordering::Relaxed),
        OPEN_CONNECTIONS.load(Ordering::Relaxed),
        MAX_PROCESSES - PROCESS_SLOTS.available_permits(),
        MAX_MESSAGES,
        MAX_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn admission_counts_processing_and_cancellation_releases_waiting_decoders() {
        let budget = IncomingBudget::new(1, 512);
        let held = budget.admit(512).await.unwrap();
        let waiting_budget = budget.clone();
        let waiting = tokio::spawn(async move { waiting_budget.admit(128).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while budget.waiting_messages.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(budget.admitted_messages.load(Ordering::Relaxed), 1);
        assert_eq!(budget.admitted_bytes.load(Ordering::Relaxed), 512);
        assert_eq!(budget.waiting_messages.load(Ordering::Relaxed), 1);
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(budget.waiting_messages.load(Ordering::Relaxed), 0);
        assert_eq!(budget.waiting_bytes.load(Ordering::Relaxed), 0);
        drop(held);
        let permit = tokio::time::timeout(Duration::from_secs(1), budget.admit(512))
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(budget.admitted_messages.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn owned_snapshot_and_reply_growth_share_the_byte_budget() {
        let budget = IncomingBudget::new(4, 1024);
        let mut first = budget.admit(128).await.unwrap();
        let second = budget.admit(512).await.unwrap();
        assert!(!first.try_extend(512));
        assert_eq!(budget.admitted_bytes.load(Ordering::Relaxed), 640);
        drop(second);
        assert!(first.try_extend(512));
        assert_eq!(budget.admitted_bytes.load(Ordering::Relaxed), 640);
        drop(first);
        assert_eq!(budget.admitted_bytes.load(Ordering::Relaxed), 0);
        assert!(budget.admit(1024).await.is_ok());
    }

    #[test]
    fn repeated_empty_fields_are_charged_for_decoded_vector_storage() {
        use fastab_proto::figterm::UpdateShellContextRequest;
        let message = FigtermRequestMessage {
            request: Some(figterm_request_message::Request::UpdateShellContext(
                UpdateShellContextRequest {
                    environment_variables: vec![
                        EnvironmentVariable {
                            key: String::new(),
                            value: None
                        };
                        1024
                    ],
                    ..Default::default()
                },
            )),
        };
        assert!(local_size(&message) > message.encoded_len() + 1024 * size_of::<EnvironmentVariable>());
    }
}
