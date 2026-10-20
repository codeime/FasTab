use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use fastab_ipc::{BufferedReader, RecvMessage};
use fastab_proto::local::ShellContext;
use fastab_proto::remote::clientbound::{self, HandshakeResponse};
use fastab_proto::remote::{Clientbound, Hostbound, hostbound};
use fastab_util::PTY_BINARY_NAME;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, Instant, MissedTickBehavior};
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::RemoteHookHandler;
use crate::figterm::{EditBuffer, FigtermSession, FigtermState, InterceptMode};
use crate::outbox::{self, FigtermSender, Outbox};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Every terminal tab or pane holds one `fterm` connection for its lifetime,
/// so a session past this cap gets no completions at all. launchd starts the
/// app with a 256-descriptor soft limit; keep half for SQLite, logs, Metal and
/// the pipes of running hook scripts.
const MAX_REMOTE_CONNECTIONS: usize = 128;
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Accepted sockets, including authentication, blocked hooks and cleanup.
/// The listener's one permit reserved before accept is not an open connection.
pub fn active_connections() -> usize {
    ACTIVE_CONNECTIONS.load(Ordering::Relaxed)
}

struct ConnectionAdmission {
    _permit: OwnedSemaphorePermit,
}

impl ConnectionAdmission {
    fn new(permit: OwnedSemaphorePermit) -> Self {
        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        Self { _permit: permit }
    }
}

impl Drop for ConnectionAdmission {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        // The permit remains owned through session_closed/sessions_changed,
        // so a stalled host cannot collect detached cleanup-hook waiters.
    }
}

/// Own the background tasks and session even while an awaited hook is pending.
/// Aborting the parent future must not detach a live writer or heartbeat loop.
struct RemoteConnection {
    sender: FigtermSender,
    state: Arc<FigtermState>,
    session_id: Uuid,
    tasks: tokio::task::JoinSet<()>,
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        self.sender.close();
        let _ = self.state.remove_id(&self.session_id);
        // JoinSet's Drop aborts any child that normal cleanup has not joined.
        // Async hook notifications run only in the normal cleanup path below.
    }
}

pub async fn start_remote_ipc(
    socket_path: PathBuf,
    figterm_state: Arc<FigtermState>,
    hook: impl RemoteHookHandler + Send + Clone + 'static,
) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).context("Failed creating socket path")?;
        }

        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, Permissions::from_mode(0o700))?;
        }
    }

    tokio::fs::remove_file(&socket_path).await.ok();

    let listener = UnixListener::bind(socket_path)?;

    serve_remote_listener(listener, figterm_state, hook, MAX_REMOTE_CONNECTIONS).await;

    Ok(())
}

async fn serve_remote_listener(
    listener: UnixListener,
    figterm_state: Arc<FigtermState>,
    hook: impl RemoteHookHandler + Send + Clone + 'static,
    limit: usize,
) {
    let slots = Arc::new(Semaphore::new(limit));
    let mut accept_backoff = None;
    while let Ok(permit) = slots.clone().acquire_owned().await {
        // EMFILE and friends are transient. Ending the loop would leave every
        // later terminal without completions until the app restarts.
        let stream = match listener.accept().await {
            Ok((stream, _)) => {
                accept_backoff = None;
                stream
            },
            Err(err) => {
                // Back off so a lasting EMFILE does not flood the rolling log.
                let delay = accept_backoff.map_or(Duration::from_millis(50), |delay: Duration| {
                    (delay * 2).min(Duration::from_secs(1))
                });
                accept_backoff = Some(delay);
                warn!(%err, ?delay, "Remote IPC accept failed");
                drop(permit);
                tokio::time::sleep(delay).await;
                continue;
            },
        };
        let admission = ConnectionAdmission::new(permit);
        let state = figterm_state.clone();
        let hook = hook.clone();
        tokio::spawn(async move {
            let _admission = admission;
            handle_remote_ipc(stream, state, hook).await;
        });
    }
}

pub async fn handle_remote_ipc(
    stream: UnixStream,
    figterm_state: Arc<FigtermState>,
    mut hook: impl RemoteHookHandler + Send,
) {
    let (reader, writer) = tokio::io::split(stream);
    let (clientbound_tx, clientbound_rx) = outbox::channel();
    let mut initialized = false;
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let session_id = Uuid::new_v4();
    let mut connection = RemoteConnection {
        sender: clientbound_tx.clone(),
        state: figterm_state.clone(),
        session_id,
        tasks: tokio::task::JoinSet::new(),
    };
    connection
        .tasks
        .spawn(handle_outgoing(writer, clientbound_rx, clientbound_tx.clone()));
    connection.tasks.spawn(send_pings(clientbound_tx.clone()));
    let maintenance = clientbound_tx.clone();
    connection.tasks.spawn(async move {
        maintenance.maintain_responses().await;
    });

    let mut reader = BufferedReader::new(reader);
    // A hook can itself await work. Retirement must also cancel that wait,
    // rather than being observed only after the next reader select iteration.
    tokio::select! {
        biased;
        _ = clientbound_tx.closed() => {},
        _ = async {
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(handshake_deadline), if !initialized => {
                debug!(?session_id, "Remote handshake timed out");
                break;
            }
            _ = clientbound_tx.closed() => {
                debug!("Connection closed");
                break;
            }
            message = reader.recv_message::<Hostbound>() => match message {
                Ok(Some(message)) => {
                    trace!(?message, "Received remote message");
                    if let Some(response) = match message.packet {
                        Some(hostbound::Packet::Handshake(handshake)) => {
                            let result = if initialized {
                                // maybe they missed our response, but they should've been listening harder
                                Some(clientbound::Packet::HandshakeResponse(HandshakeResponse {
                                    success: false,
                                }))
                            } else {
                                initialized = true;
                                debug!(id = %handshake.id, "Client auth accepted for new session");
                                figterm_state.insert(FigtermSession {
                                    id: session_id,
                                    secret: handshake.secret.clone(),
                                    sender: clientbound_tx.clone(),
                                    dead_since: None,
                                    last_receive: Instant::now(),
                                    edit_buffer: EditBuffer {
                                        text: "".to_string(),
                                        cursor: 0,
                                    },
                                    context: None,
                                    flattened_env: Arc::new(Vec::new()),
                                    terminal_cursor_coordinates: None,
                                    current_session_metrics: None,
                                    intercept: InterceptMode::Unlocked,
                                    intercept_global: InterceptMode::Unlocked
                                });
                                Some(clientbound::Packet::HandshakeResponse(HandshakeResponse {
                                    success: true,
                                }))
                            };

                            if matches!(result, Some(clientbound::Packet::HandshakeResponse(HandshakeResponse { success: true }))) {
                                hook.sessions_changed(&figterm_state).await;

                                if let Some(parent_id) = handshake.parent_id {
                                    let inner = figterm_state.inner.lock();
                                    let sessions = inner.linked_sessions.values();
                                    for session in sessions {
                                        {
                                            let notification = clientbound::Packet::NotifyChildSessionStarted(
                                                clientbound::NotifyChildSessionStarted { parent_id: parent_id.clone() }
                                            );
                                            session.sender.send_packet(
                                                Clientbound {
                                                    packet: Some(notification)
                                                }
                                            ).ok();
                                        }
                                    }
                                }
                            }

                            result
                        },
                        Some(hostbound::Packet::Request(hostbound::Request { request: Some(request), nonce })) => {
                            // Every hook, including PostExec, belongs to an
                            // authenticated session. No pre-handshake hook may
                            // await host admission outside the timer select.
                            if !initialized {
                                debug!("Client tried to send remote hook without auth");
                                Some(clientbound::Packet::HandshakeResponse(HandshakeResponse {
                                    success: false,
                                }))
                            } else {
                                /*
                                    WARNING, when adding new remote requests you must sanitize the context,
                                    otherwise the client can forge a message from another session
                                */
                                let res = match request {
                                    hostbound::request::Request::EditBuffer(mut edit_buffer) => {
                                        sanitize_fn(&mut edit_buffer.context, session_id);
                                        if let Some(shell_context) = &edit_buffer.context {
                                            hook.shell_context(shell_context, session_id).await;
                                        }
                                        hook.edit_buffer(
                                            &edit_buffer,
                                            session_id,
                                            &figterm_state,
                                        )
                                        .await
                                    },
                                    hostbound::request::Request::Prompt(mut prompt) => {
                                        sanitize_fn(&mut prompt.context, session_id);
                                        if let Some(shell_context) = &prompt.context {
                                            hook.shell_context(shell_context, session_id).await;
                                        }
                                        hook.prompt(&prompt, session_id, &figterm_state).await
                                    },
                                    hostbound::request::Request::PreExec(mut pre_exec) => {
                                        sanitize_fn(&mut pre_exec.context, session_id);
                                        if let Some(shell_context) = &pre_exec.context {
                                            hook.shell_context(shell_context, session_id).await;
                                        }
                                        hook.pre_exec(&pre_exec, session_id, &figterm_state).await
                                    },
                                    hostbound::request::Request::PostExec(mut post_exec) => {
                                        sanitize_fn(&mut post_exec.context, session_id);
                                        if let Some(shell_context) = &post_exec.context {
                                            hook.shell_context(shell_context, session_id).await;
                                        }
                                        hook.post_exec(&post_exec, session_id, &figterm_state).await
                                    },
                                    hostbound::request::Request::InterceptedKey(mut intercepted_key) => {
                                        sanitize_fn(&mut intercepted_key.context, session_id);
                                        if let Some(shell_context) = &intercepted_key.context {
                                            hook.shell_context(shell_context, session_id).await;
                                        }
                                        hook.intercepted_key(intercepted_key, session_id).await
                                    },
                                } ;

                                match res {
                                    Ok(inner) => inner.map(|inner| clientbound::Packet::Response(clientbound::Response { nonce, response: Some(inner) })),
                                    Err(err) => {
                                        error!(%err, "Failed processing hook");
                                        None
                                    }
                                }
                            }
                        },
                        Some(hostbound::Packet::Response(hostbound::Response {
                            nonce,
                            response: Some(response),
                        })) => {
                            if initialized {
                                if let Some(nonce) = nonce {
                                    clientbound_tx.respond(nonce, response);
                                }
                            }
                            None
                        },
                        Some(hostbound::Packet::Pong(())) => {
                            trace!(?session_id, "Received pong");
                            figterm_state.with(&session_id, |session| {
                                session.last_receive = Instant::now();
                            });
                            None
                        },
                        Some(hostbound::Packet::Request(hostbound::Request { request: None, .. })
                            | hostbound::Packet::Response(hostbound::Response { response: None, .. }))
                            | None => {
                            warn!(?message.packet, "Received unknown remote packet");
                            None
                        }
                    } {
                        let _ = clientbound_tx.send_packet(Clientbound { packet: Some(response) });
                    }
                }
                Ok(None) => {
                    debug!("{PTY_BINARY_NAME} connection closed");
                    break;
                }
                Err(err) => {
                    if !err.is_disconnect() {
                        warn!(%err, "Failed receiving remote message");
                    }
                    break;
                }
            }
        }
    }

        } => {},
    }
    clientbound_tx.close();

    // Retire both socket halves and background tasks before cleanup hooks can
    // await arbitrary work. A pending notification must not keep a PTY bound.
    drop(reader);
    while let Some(result) = connection.tasks.join_next().await {
        if let Err(err) = result {
            error!(%err, "remote connection task join error");
        }
    }

    if figterm_state.remove_id(&session_id).is_some() {
        hook.session_closed(session_id).await;
        hook.sessions_changed(&figterm_state).await;
    }

    info!("Disconnect from {session_id:?}");
}

async fn handle_outgoing(
    mut writer: tokio::io::WriteHalf<UnixStream>,
    mut outgoing: Outbox,
    connection: FigtermSender,
) {
    while let Some(frame) = outgoing.recv().await {
        // Cancellation of a partially written frame retires the entire socket.
        // Never return to the queue after interrupting write_all.
        let sent = tokio::select! {
            biased;
            _ = connection.closed() => break,
            result = tokio::time::timeout(WRITE_TIMEOUT, async {
                writer.write_all(frame.bytes()).await?;
                writer.flush().await
            }) => result,
        };
        match sent {
            Ok(Ok(())) => {},
            Ok(Err(error)) => {
                debug!(%error, "remote outgoing write failed");
                break;
            },
            Err(_) => {
                debug!("remote outgoing write timed out");
                break;
            },
        }
    }
    connection.close();
    // Dropping both halves in the parent closes the connection; no shutdown
    // await is needed (and a stalled writer must not delay retirement).
}

async fn send_pings(outgoing: FigtermSender) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = outgoing.closed() => break,
            _ = interval.tick() => {
                // Keep the original heartbeat for old peers and forwarded
                // connections. There is no negotiated capability to omit it.
                // Its write deadline still detects a peer with a closed read
                // half, even while no application frame is being sent.
                if outgoing.send_packet(Clientbound {
                    packet: Some(clientbound::Packet::Ping(())),
                }).is_err() {
                    break;
                }
            }
        }
    }
}

// This has to be used to sanitize as a hook can contain an invalid session_id and it must
// be sanitized before being sent to any consumers
fn sanitize_fn(context: &mut Option<ShellContext>, session_id: Uuid) {
    if let Some(context) = context {
        context.session_id = Some(session_id.to_string());
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::figterm::FigtermCommand;
    use fastab_ipc::SendMessage;
    use fastab_proto::local::{EditBufferHook, InterceptedKeyHook, PostExecHook, PreExecHook, PromptHook};

    #[derive(Debug)]
    enum Lifecycle {
        Changed(Vec<Uuid>),
        Closed(Uuid),
        HookBlocked(Uuid),
    }

    #[derive(Clone)]
    struct Hook {
        events: tokio::sync::mpsc::UnboundedSender<Lifecycle>,
        block_edit: bool,
        cleanup_release: Option<Arc<tokio::sync::Notify>>,
    }

    impl Hook {
        fn new(events: tokio::sync::mpsc::UnboundedSender<Lifecycle>) -> Self {
            Self {
                events,
                block_edit: false,
                cleanup_release: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl RemoteHookHandler for Hook {
        type Error = anyhow::Error;
        async fn sessions_changed(&mut self, state: &Arc<FigtermState>) {
            let ids = state.inner.lock().linked_sessions.keys().copied().collect();
            self.events.send(Lifecycle::Changed(ids)).unwrap();
        }
        async fn session_closed(&mut self, id: Uuid) {
            self.events.send(Lifecycle::Closed(id)).unwrap();
            if let Some(release) = &self.cleanup_release {
                release.notified().await;
            }
        }
        async fn edit_buffer(
            &mut self,
            _: &EditBufferHook,
            id: Uuid,
            _: &Arc<FigtermState>,
        ) -> Result<Option<clientbound::response::Response>> {
            if self.block_edit {
                self.events.send(Lifecycle::HookBlocked(id)).unwrap();
                std::future::pending::<()>().await;
            }
            Ok(None)
        }
        async fn prompt(
            &mut self,
            _: &PromptHook,
            _: Uuid,
            _: &Arc<FigtermState>,
        ) -> Result<Option<clientbound::response::Response>> {
            Ok(None)
        }
        async fn pre_exec(
            &mut self,
            _: &PreExecHook,
            _: Uuid,
            _: &Arc<FigtermState>,
        ) -> Result<Option<clientbound::response::Response>> {
            Ok(None)
        }
        async fn post_exec(
            &mut self,
            _: &PostExecHook,
            _: Uuid,
            _: &Arc<FigtermState>,
        ) -> Result<Option<clientbound::response::Response>> {
            Ok(None)
        }
        async fn intercepted_key(
            &mut self,
            _: InterceptedKeyHook,
            _: Uuid,
        ) -> Result<Option<clientbound::response::Response>> {
            Ok(None)
        }
    }

    async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Lifecycle>) -> Lifecycle {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn listener_admission_includes_blocked_session_cleanup() {
        let path = std::env::temp_dir().join(format!("fi-{}.sock", &Uuid::new_v4().simple().to_string()[..8]));
        let listener = UnixListener::bind(&path).unwrap();
        let state = Arc::new(FigtermState::new());
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let hook = Hook {
            cleanup_release: Some(release.clone()),
            ..Hook::new(tx)
        };
        let server = tokio::spawn(serve_remote_listener(listener, state.clone(), hook, 1));
        let handshake = || Hostbound {
            packet: Some(hostbound::Packet::Handshake(hostbound::Handshake {
                id: "limited-session".into(),
                secret: "fixture".into(),
                parent_id: None,
            })),
        };
        let mut first = UnixStream::connect(&path).await.unwrap();
        first.send_message(handshake()).await.unwrap();
        assert!(matches!(next_event(&mut events).await, Lifecycle::Changed(ids) if ids.len() == 1));
        let mut second = UnixStream::connect(&path).await.unwrap();
        second.send_message(handshake()).await.unwrap();
        drop(first);
        assert!(matches!(next_event(&mut events).await, Lifecycle::Closed(_)));
        assert!(state.inner.lock().linked_sessions.is_empty());
        // The old socket is already retired, but its blocked cleanup still
        // owns admission. No second decoded hook or extra cleanup task exists.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), events.recv())
                .await
                .is_err()
        );
        release.notify_one();
        assert!(matches!(next_event(&mut events).await, Lifecycle::Changed(ids) if ids.is_empty()));
        assert!(matches!(next_event(&mut events).await, Lifecycle::Changed(ids) if ids.len() == 1));
        drop(second);
        assert!(matches!(next_event(&mut events).await, Lifecycle::Closed(_)));
        release.notify_one();
        assert!(matches!(next_event(&mut events).await, Lifecycle::Changed(ids) if ids.is_empty()));
        server.abort();
        let _ = server.await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn silent_unauthenticated_connection_expires_without_session_notifications() {
        let state = Arc::new(FigtermState::new());
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (_client, stream) = UnixStream::pair().unwrap();
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT + Duration::from_secs(1),
            handle_remote_ipc(stream, state.clone(), Hook::new(tx)),
        )
        .await
        .expect("authentication deadline must retire an otherwise live socket");
        assert!(state.inner.lock().linked_sessions.is_empty());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn post_exec_cannot_enter_a_hook_before_authentication() {
        let state = Arc::new(FigtermState::new());
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (client, stream) = UnixStream::pair().unwrap();
        let server = tokio::spawn(handle_remote_ipc(stream, state.clone(), Hook::new(tx)));
        let mut client = BufferedReader::new(client);
        client
            .send_message(Hostbound {
                packet: Some(hostbound::Packet::Request(hostbound::Request {
                    nonce: None,
                    request: Some(hostbound::request::Request::PostExec(PostExecHook::default())),
                })),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match client.recv_message::<Clientbound>().await.unwrap().unwrap().packet {
                    Some(clientbound::Packet::Ping(())) => {},
                    Some(clientbound::Packet::HandshakeResponse(response)) => {
                        assert!(!response.success);
                        break;
                    },
                    other => panic!("unexpected unauthenticated response: {other:?}"),
                }
            }
        })
        .await
        .expect("pre-handshake PostExec must be rejected before hook dispatch");
        drop(client);
        server.await.unwrap();
        assert!(state.inner.lock().linked_sessions.is_empty());
        assert!(events.try_recv().is_err());
    }

    struct BlockedConnection {
        client: BufferedReader<UnixStream>,
        server: tokio::task::JoinHandle<()>,
        state: Arc<FigtermState>,
        session_id: Uuid,
        events: tokio::sync::mpsc::UnboundedReceiver<Lifecycle>,
    }

    impl BlockedConnection {
        async fn new(cleanup_release: Option<Arc<tokio::sync::Notify>>) -> Self {
            let state = Arc::new(FigtermState::new());
            let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
            let (client, server) = UnixStream::pair().unwrap();
            let server = tokio::spawn(handle_remote_ipc(
                server,
                state.clone(),
                Hook {
                    events: tx,
                    block_edit: true,
                    cleanup_release,
                },
            ));
            let mut client = BufferedReader::new(client);
            client
                .send_message(Hostbound {
                    packet: Some(hostbound::Packet::Handshake(hostbound::Handshake {
                        id: "blocked-hook".into(),
                        secret: "fixture".into(),
                        parent_id: None,
                    })),
                })
                .await
                .unwrap();
            let Lifecycle::Changed(ids) = next_event(&mut events).await else {
                panic!("session created")
            };
            let [session_id] = ids.as_slice() else {
                panic!("one session")
            };
            let session_id = *session_id;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match client.recv_message::<Clientbound>().await.unwrap().unwrap().packet {
                        Some(clientbound::Packet::HandshakeResponse(response)) => {
                            assert!(response.success);
                            break;
                        },
                        Some(clientbound::Packet::Ping(())) => {},
                        packet => panic!("unexpected handshake packet: {packet:?}"),
                    }
                }
            })
            .await
            .unwrap();
            client
                .send_message(Hostbound {
                    packet: Some(hostbound::Packet::Request(hostbound::Request {
                        request: Some(hostbound::request::Request::EditBuffer(EditBufferHook {
                            text: "fixture".into(),
                            ..Default::default()
                        })),
                        nonce: None,
                    })),
                })
                .await
                .unwrap();
            assert!(matches!(next_event(&mut events).await, Lifecycle::HookBlocked(id) if id == session_id));
            Self {
                client,
                server,
                state,
                session_id,
                events,
            }
        }

        fn sender(&self) -> FigtermSender {
            self.state.get(&self.session_id).unwrap().sender.clone()
        }

        async fn expect_eof(&mut self) {
            use tokio::io::AsyncReadExt;
            // Drain bytes rather than framed messages: cancellation may leave
            // a partial frame, which must still be followed by actual EOF.
            let mut remaining = Vec::new();
            tokio::time::timeout(Duration::from_secs(3), self.client.read_to_end(&mut remaining))
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn close_cancels_a_blocked_hook_and_retires_the_socket_before_cleanup_hooks() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut connection = BlockedConnection::new(Some(release.clone())).await;
        connection.sender().close();
        assert!(
            matches!(next_event(&mut connection.events).await, Lifecycle::Closed(id) if id == connection.session_id)
        );
        assert!(connection.state.get(&connection.session_id).is_none());
        assert!(!connection.server.is_finished(), "cleanup hook should still be blocked");
        connection.expect_eof().await;
        release.notify_one();
        assert!(matches!(next_event(&mut connection.events).await, Lifecycle::Changed(ids) if ids.is_empty()));
        tokio::time::timeout(Duration::from_secs(3), connection.server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn aborting_a_parent_in_a_hook_retires_its_children_session_and_pending_rpc() {
        let mut connection = BlockedConnection::new(None).await;
        let sender = connection.sender();
        let (command, reply) = FigtermCommand::run_process("fixture".into(), vec![], None, vec![], None);
        sender.send(command).unwrap();
        connection.server.abort();
        let result = tokio::time::timeout(Duration::from_secs(3), &mut connection.server)
            .await
            .unwrap();
        assert!(result.unwrap_err().is_cancelled());
        assert!(sender.diagnostics().closed);
        assert!(connection.state.get(&connection.session_id).is_none());
        assert!(
            tokio::time::timeout(Duration::from_secs(3), reply)
                .await
                .unwrap()
                .is_err()
        );
        connection.expect_eof().await;
        assert_eq!(sender.diagnostics().pending_bytes, 0);
        assert_eq!(sender.diagnostics().pending_messages, 0);
    }

    #[tokio::test]
    async fn blocked_partial_write_is_cancelled_by_connection_close() {
        use tokio::io::AsyncReadExt;
        let (mut client, server) = UnixStream::pair().unwrap();
        let (_reader, writer) = tokio::io::split(server);
        let (sender, receiver) = outbox::channel();
        sender
            .send(FigtermCommand::SetBuffer {
                text: "x".repeat(3 * 1024 * 1024),
                cursor_position: None,
            })
            .unwrap();
        let task = tokio::spawn(handle_outgoing(writer, receiver, sender.clone()));
        let mut prefix = [0; 64];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut prefix))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&prefix[..2], b"\x1b@");
        // The peer read only a frame prefix, then stays connected without
        // reading. Close must interrupt the in-progress write, not the next one.
        sender.close();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sender.diagnostics().pending_bytes, 0);
    }

    #[tokio::test]
    async fn stopped_reader_hits_the_write_deadline_without_an_external_close() {
        let (_client, server) = UnixStream::pair().unwrap();
        let (_reader, writer) = tokio::io::split(server);
        let (sender, receiver) = outbox::channel();
        sender
            .send(FigtermCommand::SetBuffer {
                text: "x".repeat(3 * 1024 * 1024),
                cursor_position: None,
            })
            .unwrap();
        tokio::time::timeout(
            WRITE_TIMEOUT + Duration::from_secs(2),
            handle_outgoing(writer, receiver, sender.clone()),
        )
        .await
        .unwrap();
        assert!(sender.diagnostics().closed);
        assert_eq!(sender.diagnostics().pending_messages, 0);
    }

    #[tokio::test]
    async fn write_failure_closes_session_while_read_half_remains_open() {
        let state = Arc::new(FigtermState::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (client, server) = UnixStream::pair().unwrap();
        let server = server.into_std().unwrap();
        // Closing a peer's read half reports EPIPE on Linux but silently drops
        // outgoing bytes on macOS. Shut down this real socket's write half via
        // a duplicate handle to exercise the same write failure on both.
        let write_shutdown = server.try_clone().unwrap();
        let server = tokio::spawn(handle_remote_ipc(
            UnixStream::from_std(server).unwrap(),
            state.clone(),
            Hook::new(tx),
        ));
        let mut client = BufferedReader::new(client);
        client
            .send_message(Hostbound {
                packet: Some(hostbound::Packet::Handshake(hostbound::Handshake {
                    id: "write-failure".into(),
                    secret: "fixture".into(),
                    parent_id: None,
                })),
            })
            .await
            .unwrap();
        let Lifecycle::Changed(ids) = next_event(&mut rx).await else {
            panic!("expected authenticated session")
        };
        let [id] = ids.as_slice() else {
            panic!("expected one session")
        };
        let id = *id;
        // Wait for the handshake reply so ordinary output is verified before
        // the failure, ignoring the periodic ping that can arrive first.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match client.recv_message::<Clientbound>().await.unwrap().unwrap().packet {
                    Some(clientbound::Packet::HandshakeResponse(response)) => {
                        assert!(response.success);
                        break;
                    },
                    Some(clientbound::Packet::Ping(())) => {},
                    packet => panic!("unexpected handshake packet: {packet:?}"),
                }
            }
        })
        .await
        .unwrap();

        write_shutdown.shutdown(std::net::Shutdown::Write).unwrap();
        let (command, response) = FigtermCommand::run_process("fixture".into(), vec![], None, vec![], None);
        state
            .with(&id, |session| session.sender.send(command).unwrap())
            .unwrap();

        assert!(matches!(next_event(&mut rx).await, Lifecycle::Closed(closed) if closed == id));
        assert!(matches!(next_event(&mut rx).await, Lifecycle::Changed(ids) if ids.is_empty()));
        assert!(state.get(&id).is_none());
        assert!(
            tokio::time::timeout(Duration::from_secs(3), response)
                .await
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert!(rx.try_recv().is_err());
        // Keep the peer alive until cleanup finishes: EOF on the read side
        // cannot be what released the session and its pending response.
        drop(client);
    }

    #[tokio::test]
    async fn disconnect_notifies_only_the_removed_server_session_before_session_list_change() {
        let state = Arc::new(FigtermState::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hook = Hook::new(tx);
        let mut clients = Vec::new();
        let mut servers = Vec::new();
        let mut ids: Vec<Uuid> = Vec::new();
        // Reusing a shell's handshake ID still creates separate server owners.
        for _ in 0..2 {
            let (mut client, server) = UnixStream::pair().unwrap();
            servers.push(tokio::spawn(handle_remote_ipc(server, state.clone(), hook.clone())));
            client
                .send_message(Hostbound {
                    packet: Some(hostbound::Packet::Handshake(hostbound::Handshake {
                        id: "same-shell".into(),
                        secret: "fixture".into(),
                        parent_id: None,
                    })),
                })
                .await
                .unwrap();
            let Lifecycle::Changed(current) = next_event(&mut rx).await else {
                panic!("expected authenticated session")
            };
            ids.push(*current.iter().find(|id| !ids.contains(*id)).unwrap());
            clients.push(client);
        }
        assert_ne!(ids[0], ids[1]);
        for id in ids {
            drop(clients.remove(0));
            assert!(matches!(next_event(&mut rx).await, Lifecycle::Closed(closed) if closed == id));
            let Lifecycle::Changed(remaining) = next_event(&mut rx).await else {
                panic!("expected session list change")
            };
            assert!(!remaining.contains(&id));
            assert_eq!(remaining.len(), clients.len());
            tokio::time::timeout(Duration::from_secs(3), servers.remove(0))
                .await
                .unwrap()
                .unwrap();
        }
        // A connection that never authenticated did not own engine input.
        let (client, server) = UnixStream::pair().unwrap();
        let server = tokio::spawn(handle_remote_ipc(server, state, hook));
        drop(client);
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert!(rx.try_recv().is_err());
    }
}
