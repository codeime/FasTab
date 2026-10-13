//! Utiities for IPC with Tauri App

pub(crate) mod incoming;
mod outbox;

pub(crate) use outbox::{ContextAdmission, ContextProgress, Generation, GenerationSender, RemoteSender, RequestOrigin};

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use fastab_ipc::{BufferedReader, RecvMessage, SendMessage};
use fastab_proto::FigProtobufEncodable;
use fastab_proto::figterm::{FigtermRequestMessage, FigtermResponseMessage};
use fastab_proto::remote::hostbound::Handshake;
use fastab_proto::remote::{Clientbound, Hostbound, clientbound, hostbound};
use fastab_util::{PTY_BINARY_NAME, directories, gen_hex_string};
use flume::{Receiver, Sender, bounded};
use pin_project::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::time::{MissedTickBehavior, interval, timeout};
use tracing::{debug, error, info, trace};

// These use the existing connect/retry timescale. The handshake and write
// deadlines are engineering bounds, not measured remote-network optima.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct RemoteIncoming {
    pub(crate) generation: Generation,
    pub(crate) message: Clientbound,
    pub(crate) permit: incoming::IncomingPermit,
}

pub(crate) struct LocalIncoming {
    pub(crate) message: FigtermRequestMessage,
    pub(crate) reply: LocalReply,
}

pub(crate) struct LocalReply {
    completion: oneshot::Sender<Option<LocalResponse>>,
    permit: incoming::IncomingPermit,
    serial: OwnedSemaphorePermit,
}

struct LocalResponse {
    message: FigtermResponseMessage,
    _permit: incoming::IncomingPermit,
    _serial: OwnedSemaphorePermit,
}

impl LocalReply {
    pub(crate) fn complete(mut self, response: Option<FigtermResponseMessage>) {
        if let Some(response) = &response {
            if !self.permit.try_extend(incoming::response_size(response)) {
                // Dropping the reserved sender makes the writer retire this
                // connection. Never silently turn a rejected reply into None.
                debug!("Local IPC response exceeds available admission budget");
                return;
            }
        }
        let response = response.map(|message| LocalResponse {
            message,
            _permit: self.permit,
            _serial: self.serial,
        });
        // The reader reserved this one-shot slot before admitting the request.
        // Never wait for the socket writer from the PTY's main loop.
        let _ = self.completion.send(response);
    }

    #[cfg(test)]
    fn is_closed(&self) -> bool {
        self.completion.is_closed()
    }
}

struct ForwardedConnection {
    reader: MessageSource,
    writer: MessageSink,
    child: Option<Child>,
}

#[allow(dead_code)]
#[pin_project(project = MessageSourceProj)]
enum MessageSource {
    UnixStream(#[pin] tokio::io::ReadHalf<tokio::net::UnixStream>),
    ChildStdout(#[pin] ChildStdout),
}

impl AsyncRead for MessageSource {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.project() {
            MessageSourceProj::UnixStream(stream) => stream.poll_read(cx, buf),
            MessageSourceProj::ChildStdout(stdout) => stdout.poll_read(cx, buf),
        }
    }
}

#[allow(dead_code)]
#[pin_project(project = MessageSinkProj)]
enum MessageSink {
    UnixStream(#[pin] tokio::io::WriteHalf<tokio::net::UnixStream>),
    ChildStdin(#[pin] ChildStdin),
}

impl AsyncWrite for MessageSink {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            MessageSinkProj::UnixStream(stream) => stream.poll_write(cx, buf),
            MessageSinkProj::ChildStdin(stdin) => stdin.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            MessageSinkProj::UnixStream(stream) => stream.poll_flush(cx),
            MessageSinkProj::ChildStdin(stdin) => stdin.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            MessageSinkProj::UnixStream(stream) => stream.poll_shutdown(cx),
            MessageSinkProj::ChildStdin(stdin) => stdin.poll_shutdown(cx),
        }
    }
}

async fn get_forwarded_stream() -> Result<ForwardedConnection> {
    #[cfg(target_os = "linux")]
    if fastab_util::system_info::in_wsl() {
        use std::process::Stdio;

        use anyhow::Context as AnyhowContext;

        let mut child = tokio::process::Command::new("fig.exe")
            .args(["_", "stream-from-socket"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        return Ok(ForwardedConnection {
            reader: MessageSource::ChildStdout(stdout),
            writer: MessageSink::ChildStdin(stdin),
            child: Some(child),
        });
    }

    let socket = directories::remote_socket_path()?;
    let stream = fastab_ipc::socket_connect_timeout(&socket, CONNECTION_TIMEOUT).await?;
    let (reader, writer) = tokio::io::split(stream);
    Ok(ForwardedConnection {
        reader: MessageSource::UnixStream(reader),
        writer: MessageSink::UnixStream(writer),
        child: None,
    })
}

/// Spawns a local unix socket for communicating with figterm on a local machine
pub(crate) async fn spawn_figterm_ipc(session_id: impl std::fmt::Display) -> Result<Receiver<LocalIncoming>> {
    trace!("Spawning incoming receiver");

    let (incoming_tx, incoming_rx) = bounded(incoming::MAX_MESSAGES);

    let socket_path = directories::figterm_socket_path(session_id)?;
    if let Some(parent) = socket_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            error!(%err, "Failed to create {PTY_BINARY_NAME} socket directory");
        }

        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, Permissions::from_mode(0o700))?;
        }
    }

    tokio::fs::remove_file(&socket_path).await.ok();
    let socket_listener = tokio::net::UnixListener::bind(&socket_path)?;

    tokio::spawn(async move {
        loop {
            let Ok(connection) = incoming::CONNECTIONS.clone().acquire_owned().await else {
                break;
            };
            match socket_listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(handle_local_connection(
                        stream,
                        incoming_tx.clone(),
                        incoming::budget(),
                        connection,
                        CONNECTION_TIMEOUT,
                    ));
                },
                Err(err) => {
                    debug!(%err, "Local IPC accept failed");
                    drop(connection);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                },
            }
        }
    });

    Ok(incoming_rx)
}

async fn handle_local_connection(
    stream: tokio::net::UnixStream,
    incoming_tx: Sender<LocalIncoming>,
    budget: Arc<incoming::IncomingBudget>,
    _connection: OwnedSemaphorePermit,
    admission_timeout: Duration,
) {
    let _connection = incoming::ConnectionLease::new(_connection);
    let (read_half, mut write_half) = tokio::io::split(stream);
    let (response_tx, response_rx) = bounded::<oneshot::Receiver<Option<LocalResponse>>>(1);
    let serial = Arc::new(Semaphore::new(1));
    let reader = async move {
        let mut reader = BufferedReader::new(read_half);
        loop {
            match timeout(Duration::from_secs(30), reader.recv_message::<FigtermRequestMessage>()).await {
                Ok(Ok(Some(message))) => {
                    let admission = timeout(admission_timeout, async {
                        let permit = budget.admit(incoming::local_size(&message)).await?;
                        let serial = serial.clone().acquire_owned().await?;
                        let (completion, response) = oneshot::channel();
                        response_tx.send_async(response).await?;
                        let reply = LocalReply {
                            completion,
                            permit,
                            serial,
                        };
                        incoming_tx.send_async(LocalIncoming { message, reply }).await?;
                        Ok::<_, anyhow::Error>(())
                    })
                    .await;
                    if !matches!(admission, Ok(Ok(()))) {
                        debug!("Local IPC admission rejected or timed out; retiring connection");
                        break;
                    }
                },
                Ok(Ok(None)) => break,
                Ok(Err(error)) => {
                    debug!(%error, "Local IPC read failed");
                    break;
                },
                Err(_) => break,
            }
        }
    };
    let writer = async move {
        while let Ok(response) = response_rx.recv_async().await {
            let response = match response.await {
                Ok(Some(response)) => response,
                Ok(None) => continue,
                Err(_) => break,
            };
            let Ok(protobuf) = response.message.encode_fastab_protobuf() else {
                break;
            };
            if !matches!(
                timeout(CONNECTION_TIMEOUT, write_half.write_all(&protobuf)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    };
    // Both directions belong to this scope. A write failure or half-close
    // drops the other future as well; no detached reader keeps the socket alive.
    tokio::select! {
        _ = reader => {},
        _ = writer => {},
    }
}

/// Connects to the desktop app and allows for a remote connection from remote hosts
pub(crate) async fn spawn_remote_ipc(
    session_id: String,
    parent_id: Option<String>,
) -> Result<(RemoteSender, Receiver<RemoteIncoming>, oneshot::Sender<()>)> {
    let (stop_ipc_tx, mut stop_ipc_rx) = oneshot::channel::<()>();
    let outgoing = RemoteSender::new();
    let supervisor = outgoing.clone();
    let (incoming_tx, incoming_rx) = bounded::<RemoteIncoming>(incoming::MAX_MESSAGES);
    let incoming_budget = incoming::budget();

    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let secret = gen_hex_string();

        loop {
            tokio::select! {
                _ = &mut stop_ipc_rx => {
                    supervisor.stop();
                    break;
                }
                _ = interval.tick() => {}
            }
            let Some(generation) = (match supervisor.begin_attempt() {
                outbox::BeginAttempt::Started(generation) => Some(generation),
                outbox::BeginAttempt::Busy => {
                    // In-flight accounting has not cleared yet. Wait for the
                    // next reconnect tick instead of permanently stopping IPC.
                    continue;
                },
                outbox::BeginAttempt::Stopped => None,
            }) else {
                break;
            };
            let connection = tokio::select! {
                _ = &mut stop_ipc_rx => {
                    supervisor.stop();
                    break;
                }
                result = timeout(CONNECTION_TIMEOUT, get_forwarded_stream()) => match result {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(err)) => {
                        supervisor.retire(generation, &format!("connect failed: {err}"));
                        continue;
                    }
                    Err(err) => {
                        supervisor.retire(generation, &format!("connect timed out: {err}"));
                        continue;
                    }
                }
            };
            let ForwardedConnection {
                reader,
                mut writer,
                mut child,
            } = connection;
            let mut reader = BufferedReader::new(reader);
            supervisor.set_handshaking(generation);

            // Both halves are scoped to this select. Finishing either half,
            // invalidating admission, or stopping drops the other half's
            // future (including an in-flight frame) before the next generation.
            let result: Result<()> = {
                let run_connection = async {
                    timeout(CONNECTION_TIMEOUT, async {
                        writer
                            .send_message(Hostbound {
                                packet: Some(hostbound::Packet::Handshake(Handshake {
                                    id: session_id.clone(),
                                    parent_id: parent_id.clone(),
                                    secret: secret.clone(),
                                })),
                            })
                            .await?;
                        loop {
                            let Some(message) = reader.recv_message::<Clientbound>().await? else {
                                anyhow::bail!("EOF awaiting handshake");
                            };
                            if let Some(clientbound::Packet::HandshakeResponse(response)) = message.packet {
                                if !response.success {
                                    anyhow::bail!("handshake rejected");
                                }
                                return Ok::<(), anyhow::Error>(());
                            }
                        }
                    })
                    .await??;
                    supervisor.set_ready(generation);
                    info!(?generation, "Remote handshake succeeded");

                    let receive = async {
                        while let Some(message) = reader.recv_message::<Clientbound>().await? {
                            let permit = timeout(
                                CONNECTION_TIMEOUT,
                                incoming_budget.admit(incoming::remote_size(&message)),
                            )
                            .await??;
                            incoming_tx
                                .send_async(RemoteIncoming {
                                    generation,
                                    message,
                                    permit,
                                })
                                .await
                                .map_err(|err| anyhow::anyhow!("remote incoming receiver closed: {err}"))?;
                        }
                        Err::<(), anyhow::Error>(anyhow::anyhow!("remote reader reached EOF"))
                    };
                    let send = async {
                        while let Some(mut frame) = supervisor.next_frame(generation).await {
                            frame.write_to(&mut writer).await?;
                        }
                        Ok::<(), anyhow::Error>(())
                    };
                    tokio::select! {
                        result = receive => result,
                        result = send => result,
                    }
                };
                tokio::select! {
                    _ = &mut stop_ipc_rx => {
                        supervisor.stop();
                        Ok(())
                    }
                    _ = supervisor.invalidated(generation) => Ok(()),
                    status = async {
                        match child.as_mut() {
                            Some(child) => child.wait().await,
                            None => std::future::pending().await,
                        }
                    } => Err(anyhow::anyhow!("forwarder exited: {status:?}")),
                    result = run_connection => result,
                }
            };
            let reason = match result {
                Ok(()) => "connection cancelled".to_owned(),
                Err(err) => format!("connection failed: {err}"),
            };
            supervisor.retire(generation, &reason);
            drop(reader);
            drop(writer);
            if let Some(mut child) = child {
                // A WSL forwarder is owned by this connection, not by a
                // detached wait task. Close its pipes, terminate and reap it.
                if let Err(err) = child.start_kill() {
                    debug!(%err, "Could not stop remote forwarder");
                }
                let mut stopping = supervisor.phase() == outbox::ConnectionPhase::Stopped;
                let reaped = {
                    let wait = timeout(CONNECTION_TIMEOUT, child.wait());
                    tokio::pin!(wait);
                    loop {
                        tokio::select! {
                            result = &mut wait => break match result {
                                Ok(Ok(_)) => true,
                                Ok(Err(err)) => {
                                    error!(%err, "Could not confirm remote forwarder exit");
                                    false
                                }
                                Err(err) => {
                                    error!(%err, "Remote forwarder cleanup timed out; stopping IPC without reconnecting");
                                    false
                                }
                            },
                            _ = &mut stop_ipc_rx, if !stopping => {
                                stopping = true;
                                supervisor.stop();
                            }
                        }
                    }
                };
                if !reaped {
                    // kill_on_drop remains a best-effort fallback. Do not
                    // accumulate forwarders whose exit we could not confirm.
                    supervisor.stop();
                }
            }
            if supervisor.phase() == outbox::ConnectionPhase::Stopped {
                break;
            }
        }
    });

    Ok((outgoing, incoming_rx, stop_ipc_tx))
}

#[cfg(test)]
mod local_connection_tests {
    use super::*;
    use fastab_proto::figterm::{DiagnosticsRequest, figterm_request_message};

    async fn connection_slot() -> OwnedSemaphorePermit {
        Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap()
    }

    #[tokio::test]
    async fn local_peer_half_close_retires_reader_and_writer_together() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (incoming, requests) = bounded(4);
        let task = tokio::spawn(handle_local_connection(
            server,
            incoming,
            incoming::IncomingBudget::new(4, 4096),
            connection_slot().await,
            CONNECTION_TIMEOUT,
        ));
        client
            .send_message(FigtermRequestMessage {
                request: Some(figterm_request_message::Request::Diagnostics(DiagnosticsRequest {})),
            })
            .await
            .unwrap();
        // Hold a response sender past peer EOF. Its lifetime must not keep the
        // connection's writer waiting indefinitely.
        let LocalIncoming { reply: response, .. } = timeout(Duration::from_secs(2), requests.recv_async())
            .await
            .unwrap()
            .unwrap();
        client.shutdown().await.unwrap();
        timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        assert!(response.is_closed());
    }

    #[tokio::test]
    async fn dead_peer_waiting_for_admission_releases_its_connection() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (incoming, _requests) = bounded(4);
        let budget = incoming::IncomingBudget::new(1, 4096);
        let held = budget.admit(4096).await.unwrap();
        let task = tokio::spawn(handle_local_connection(
            server,
            incoming,
            budget.clone(),
            connection_slot().await,
            Duration::from_millis(30),
        ));
        client
            .send_message(FigtermRequestMessage {
                request: Some(figterm_request_message::Request::Diagnostics(DiagnosticsRequest {})),
            })
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        drop(held);
        assert!(budget.admit(4096).await.is_ok());
    }

    #[tokio::test]
    async fn local_request_replies_are_reserved_and_serialized_without_main_loop_waits() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (incoming, requests) = bounded(4);
        let task = tokio::spawn(handle_local_connection(
            server,
            incoming,
            incoming::IncomingBudget::new(4, 4096),
            connection_slot().await,
            CONNECTION_TIMEOUT,
        ));
        for _ in 0..2 {
            client
                .send_message(FigtermRequestMessage {
                    request: Some(figterm_request_message::Request::Diagnostics(DiagnosticsRequest {})),
                })
                .await
                .unwrap();
        }
        let first = timeout(Duration::from_secs(2), requests.recv_async())
            .await
            .unwrap()
            .unwrap();
        assert!(timeout(Duration::from_millis(20), requests.recv_async()).await.is_err());
        first.reply.complete(Some(FigtermResponseMessage { response: None }));
        let mut reader = BufferedReader::new(client);
        assert!(
            timeout(Duration::from_secs(2), reader.recv_message::<FigtermResponseMessage>())
                .await
                .unwrap()
                .unwrap()
                .is_some()
        );
        let second = timeout(Duration::from_secs(2), requests.recv_async())
            .await
            .unwrap()
            .unwrap();
        second.reply.complete(None);
        drop(reader);
        timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn oversized_owned_reply_retires_instead_of_blocking_or_silently_skipping() {
        use fastab_proto::figterm::{InlineShellCompletionResponse, figterm_response_message};
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (incoming, requests) = bounded(4);
        let task = tokio::spawn(handle_local_connection(
            server,
            incoming,
            incoming::IncomingBudget::new(4, 1024),
            connection_slot().await,
            CONNECTION_TIMEOUT,
        ));
        client
            .send_message(FigtermRequestMessage {
                request: Some(figterm_request_message::Request::Diagnostics(DiagnosticsRequest {})),
            })
            .await
            .unwrap();
        let request = timeout(Duration::from_secs(2), requests.recv_async())
            .await
            .unwrap()
            .unwrap();
        request.reply.complete(Some(FigtermResponseMessage {
            response: Some(figterm_response_message::Response::InlineShellCompletion(
                InlineShellCompletionResponse {
                    insert_text: Some("x".repeat(1024)),
                },
            )),
        }));
        timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        let mut reader = BufferedReader::new(client);
        assert!(reader.recv_message::<FigtermResponseMessage>().await.unwrap().is_none());
    }
}
