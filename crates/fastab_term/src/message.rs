use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alacritty_terminal::Term;
use alacritty_terminal::term::ShellState;
use anyhow::Result;
use fastab_proto::fig::EnvironmentVariable;
use fastab_proto::figterm::figterm_request_message::Request as FigtermRequest;
use fastab_proto::figterm::figterm_response_message::Response as FigtermResponse;
use fastab_proto::figterm::intercept_request::{InterceptCommand, SetFigjsIntercepts, SetFigjsVisible};
use fastab_proto::figterm::{self, FigtermRequestMessage, FigtermResponseMessage, TelemetryRequest};
use fastab_proto::remote::{Clientbound, Hostbound, clientbound, hostbound};
use fastab_util::PTY_BINARY_NAME;
use fastab_util::env_var::PROCESS_LAUNCHED_BY_Q;
use flume::Sender;
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, trace, warn};

use crate::event_handler::EventHandler;
use crate::history::HistorySender;
use crate::interceptor::KeyInterceptor;
use crate::ipc::{GenerationSender, LocalReply, RequestOrigin, incoming};
use crate::pty::{AsyncMasterPty, AsyncMasterPtyExt};
use crate::{
    INSERT_ON_NEW_CMD, INSERTION_LOCK, InsertionLock, MainLoopEvent, PendingInsertion, SHELL_ALIAS,
    SHELL_ENVIRONMENT_VARIABLES, shell_state_to_context,
};

fn working_directory(path: Option<&str>, shell_state: &ShellState) -> PathBuf {
    let map_dir = |path: PathBuf| match path.canonicalize() {
        Ok(path) if path.is_dir() => Some(path),
        Ok(path) => {
            warn!(?path, "not a directory");
            None
        },
        Err(err) => {
            warn!(?path, %err, "failed to canonicalize path");
            None
        },
    };

    path.map(PathBuf::from)
        .and_then(map_dir)
        .or_else(|| {
            shell_state
                .get_context()
                .current_working_directory
                .clone()
                .and_then(map_dir)
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| {
            cfg_if::cfg_if! {
                if #[cfg(windows)] {
                    PathBuf::from("C:\\")
                } else if #[cfg(unix)] {
                    PathBuf::from("/")
                }
            }
        })
}

fn create_command(
    executable: impl AsRef<Path>,
    working_directory: impl AsRef<Path>,
    env: &[EnvironmentVariable],
) -> Command {
    let mut cmd = if executable.as_ref().is_absolute() {
        Command::new(executable.as_ref())
    } else {
        let path = env
            .iter()
            .find_map(|var| if var.key == "PATH" { var.value.as_deref() } else { None });

        which::which_in(executable.as_ref(), path, working_directory.as_ref())
            .map_or_else(|_| Command::new(executable.as_ref()), Command::new)
    };

    #[cfg(target_os = "windows")]
    cmd.creation_flags(windows::Win32::System::Threading::DETACHED_PROCESS.0);

    cmd.current_dir(working_directory);

    #[cfg(unix)]
    {
        let pre_exec_fn = || {
            // Remove controlling terminal.
            nix::unistd::setsid()?;
            Ok(())
        };

        // SAFETY: this closure is run after forking the process and only affects the
        // child. setsid is async-signal-safe.
        unsafe { cmd.pre_exec(pre_exec_fn) };
    }

    if env.iter().any(|var| var.value.is_some()) {
        cmd.env_clear();
        cmd.envs(
            env.iter()
                .filter_map(|var| var.value.as_ref().map(|value| (&var.key, value))),
        );
    }

    cmd.env_remove("LS_COLORS");
    cmd.env_remove("CLICOLOR_FORCE");
    cmd.env_remove("CLICOLOR");
    cmd.env_remove("COLORTERM");
    cmd.envs([
        (PROCESS_LAUNCHED_BY_Q, "1"),
        ("HISTFILE", ""),
        ("HISTCONTROL", "ignoreboth"),
        ("TERM", "xterm-256color"),
        ("NO_COLOR", "1"),
    ]);

    cmd.kill_on_drop(true);

    cmd
}

async fn acquire_process_slot(slots: Arc<Semaphore>, sender: &GenerationSender) -> Option<OwnedSemaphorePermit> {
    tokio::select! {
        biased;
        _ = sender.invalidated() => None,
        slot = slots.acquire_owned() => slot.ok(),
    }
}

/// Process the inner figterm request enum, shared between local and remote
pub(crate) async fn process_figterm_request(
    figterm_request: FigtermRequest,
    _main_loop_tx: Sender<MainLoopEvent>,
    term: &Term<EventHandler>,
    pty_master: &mut Box<dyn AsyncMasterPty + Send + Sync>,
    key_interceptor: &mut KeyInterceptor,
    origin: RequestOrigin,
) -> Result<Option<FigtermResponse>> {
    match figterm_request {
        FigtermRequest::InsertText(request) => {
            // If the shell is in prompt or a command is being executed, insert the text only
            // if the insert during command option is enabled.
            if term.shell_state().preexec && !request.insert_during_command() {
                return Ok(None);
            }

            let current_buffer = term.get_current_buffer().map(|buff| (buff.buffer, buff.cursor_idx));
            let mut insertion_string = String::new();
            if let Some((buffer, Some(position))) = current_buffer {
                if let Some(ref text_to_insert) = request.insertion {
                    trace!(?buffer, ?position);

                    // perform deletion
                    // if let Some(deletion) = command.deletion {
                    //     let deletion = deletion as usize;
                    //     buffer.drain(position - deletion..position);
                    // }
                    // // move cursor
                    // if let Some(offset) = command.offset {
                    //     position += offset as usize;
                    // }
                    // // split text by cursor
                    // let (left, right) = buffer.split_at(position);

                    let expected = format!("{buffer}{text_to_insert}");
                    trace!(?expected, "lock set, expected buffer");
                    *INSERTION_LOCK.lock().unwrap() = Some(InsertionLock {
                        at: SystemTime::now(),
                        expected,
                        origin,
                    });
                }
                if let Some(ref insertion_buffer) = request.insertion_buffer {
                    if buffer.ne(insertion_buffer) {
                        if buffer.starts_with(insertion_buffer) {
                            if let Some(len_diff) = buffer.len().checked_sub(insertion_buffer.len()) {
                                insertion_string.extend(std::iter::repeat_n('\x08', len_diff));
                            }
                        } else if insertion_buffer.starts_with(&buffer) {
                            insertion_string.push_str(&insertion_buffer[buffer.len()..]);
                        }
                    }
                }
            }
            insertion_string.push_str(&request.to_term_string());
            pty_master.write_all(insertion_string.as_bytes()).await?;
            Ok(None)
        },
        FigtermRequest::Intercept(request) => {
            match request.intercept_command {
                Some(InterceptCommand::SetFigjsIntercepts(SetFigjsIntercepts {
                    intercept_bound_keystrokes,
                    intercept_global_keystrokes,
                    actions,
                    override_actions,
                })) => {
                    key_interceptor.set_intercepts_from(
                        intercept_global_keystrokes,
                        intercept_bound_keystrokes,
                        origin,
                    );
                    key_interceptor.set_actions(&actions, override_actions);
                },
                Some(InterceptCommand::SetFigjsVisible(SetFigjsVisible { visible })) => {
                    key_interceptor.set_window_visible_from(visible, origin);
                },
                None => {},
            }

            Ok(None)
        },
        FigtermRequest::Diagnostics(_) => {
            let map_color = |color: &shell_color::VTermColor| -> figterm::TermColor {
                figterm::TermColor {
                    color: Some(match color {
                        shell_color::VTermColor::Rgb { red, green, blue } => {
                            figterm::term_color::Color::Rgb(figterm::term_color::Rgb {
                                r: *red as i32,
                                b: *blue as i32,
                                g: *green as i32,
                            })
                        },
                        shell_color::VTermColor::Indexed { idx } => figterm::term_color::Color::Indexed(*idx as u32),
                    }),
                }
            };

            let map_style = |style: &shell_color::SuggestionColor| -> figterm::TermStyle {
                figterm::TermStyle {
                    fg: style.fg().as_ref().map(map_color),
                    bg: style.bg().as_ref().map(map_color),
                }
            };

            let (edit_buffer, cursor_position) = term.get_current_buffer().map_or((None, None), |buf| {
                (Some(buf.buffer), buf.cursor_idx.and_then(|i| i.try_into().ok()))
            });

            let response = FigtermResponse::Diagnostics(figterm::DiagnosticsResponse {
                shell_context: Some(shell_state_to_context(term.shell_state())),
                fish_suggestion_style: term.shell_state().fish_suggestion_color.as_ref().map(map_style),
                zsh_autosuggestion_style: term.shell_state().zsh_autosuggestion_color.as_ref().map(map_style),
                edit_buffer,
                cursor_position,
                build_identity_json: Some(fastab_util::build_identity::json("fterm")),
                resource_diagnostics_json: Some(crate::resource_diagnostics::json()),
            });

            Ok(Some(response))
        },
        FigtermRequest::InsertOnNewCmd(command) => {
            *INSERT_ON_NEW_CMD.lock().unwrap() = Some(PendingInsertion {
                text: command.text,
                bracketed: command.bracketed,
                execute: command.execute,
                origin,
            });
            Ok(None)
        },
        FigtermRequest::SetBuffer(_) => Err(anyhow::anyhow!("SetBuffer is not supported in figterm")),
        FigtermRequest::UpdateShellContext(request) => {
            let mut updated = false;
            if request.update_environment_variables {
                *SHELL_ENVIRONMENT_VARIABLES.lock().unwrap() = Arc::new(request.environment_variables);
                updated = true;
            }
            if request.update_alias {
                *SHELL_ALIAS.lock().unwrap() = request.alias;
                updated = true;
            }
            if updated {
                crate::note_shell_context_updated();
            }
            Ok(None)
        },
        // Local dispatch applies this through MainLoopControl. Never send
        // synchronously into the bounded queue consumed by this same loop.
        FigtermRequest::NotifySshSessionStarted(_) => Ok(None),
        FigtermRequest::Telemtety(_) => anyhow::bail!("Telemetry is not supported over remote"),
        FigtermRequest::InlineShellCompletion(_)
        | FigtermRequest::InlineShellCompletionAccept(_)
        | FigtermRequest::InlineShellCompletionSetEnabled(_) => Ok(None),
    }
}

/// Process a figterm request message
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_figterm_message(
    figterm_request_message: FigtermRequestMessage,
    main_loop_tx: Sender<MainLoopEvent>,
    reply: LocalReply,
    term: &Term<EventHandler>,
    _history_sender: &HistorySender,
    pty_master: &mut Box<dyn AsyncMasterPty + Send + Sync>,
    key_interceptor: &mut KeyInterceptor,
    _session_id: &str,
) -> Result<()> {
    let response = match figterm_request_message.request {
        Some(
            FigtermRequest::InlineShellCompletion(_)
            | FigtermRequest::InlineShellCompletionAccept(_)
            | FigtermRequest::InlineShellCompletionSetEnabled(_),
        ) => None,
        Some(FigtermRequest::Telemtety(TelemetryRequest { .. })) => None,
        Some(request) => {
            match process_figterm_request(
                request,
                main_loop_tx,
                term,
                pty_master,
                key_interceptor,
                RequestOrigin::Local,
            )
            .await
            {
                Ok(Some(response)) => Some(FigtermResponseMessage {
                    response: Some(response),
                }),
                Ok(None) => None,
                Err(err) => {
                    error!(%err, "Failed to process figterm message");
                    None
                },
            }
        },
        None => {
            warn!("{PTY_BINARY_NAME} message with no request");
            None
        },
    };
    reply.complete(response);
    Ok(())
}

fn send_figterm_response_hostbound(
    response: Option<FigtermResponse>,
    nonce: Option<u64>,
    response_tx: &GenerationSender,
) {
    use hostbound::response::Response;

    if let Some(response) = response {
        let hostbound = Hostbound {
            packet: Some(hostbound::Packet::Response(hostbound::Response {
                nonce,
                response: Some(match response {
                    FigtermResponse::Diagnostics(diagnostics) => Response::Diagnostics(diagnostics),
                    FigtermResponse::InlineShellCompletion(_) => return,
                }),
            })),
        };

        let _ = response_tx.try_send(hostbound);
    }
}

pub(crate) async fn process_remote_message(
    clientbound_message: Clientbound,
    main_loop_tx: Sender<MainLoopEvent>,
    response_tx: GenerationSender,
    mut incoming_permit: incoming::IncomingPermit,
    term: &Term<EventHandler>,
    pty_master: &mut Box<dyn AsyncMasterPty + Send + Sync>,
    key_interceptor: &mut KeyInterceptor,
) -> Result<()> {
    use clientbound::request::Request;
    use hostbound::response::Response;

    // Queued inbound messages and their replies belong to one connection.
    // Applying old interception/insertion after reconnect would be stale too.
    if !response_tx.is_current_ready() {
        return Ok(());
    }
    let origin = RequestOrigin::Remote(response_tx.generation());

    match clientbound_message.packet {
        Some(clientbound::Packet::Request(request)) => {
            let nonce = request.nonce;
            let make_response = move |response: Response| -> Hostbound {
                Hostbound {
                    packet: Some(hostbound::Packet::Response(hostbound::Response {
                        response: Some(response),
                        nonce,
                    })),
                }
            };

            match request.request {
                Some(Request::InsertText(request)) => {
                    send_figterm_response_hostbound(
                        process_figterm_request(
                            FigtermRequest::InsertText(request),
                            main_loop_tx,
                            term,
                            pty_master,
                            key_interceptor,
                            origin,
                        )
                        .await?,
                        nonce,
                        &response_tx,
                    );
                },
                Some(Request::Intercept(request)) => {
                    send_figterm_response_hostbound(
                        process_figterm_request(
                            FigtermRequest::Intercept(request),
                            main_loop_tx,
                            term,
                            pty_master,
                            key_interceptor,
                            origin,
                        )
                        .await?,
                        nonce,
                        &response_tx,
                    );
                },
                Some(Request::Diagnostics(request)) => {
                    send_figterm_response_hostbound(
                        process_figterm_request(
                            FigtermRequest::Diagnostics(request),
                            main_loop_tx,
                            term,
                            pty_master,
                            key_interceptor,
                            origin,
                        )
                        .await?,
                        nonce,
                        &response_tx,
                    );
                },
                Some(Request::InsertOnNewCmd(request)) => {
                    send_figterm_response_hostbound(
                        process_figterm_request(
                            FigtermRequest::InsertOnNewCmd(request),
                            main_loop_tx,
                            term,
                            pty_master,
                            key_interceptor,
                            origin,
                        )
                        .await?,
                        nonce,
                        &response_tx,
                    );
                },
                Some(Request::RunProcess(request)) => {
                    let directory = working_directory(request.working_directory.as_deref(), term.shell_state());
                    // Freeze at main-loop dispatch, as before, and charge every
                    // retained epoch. Never wait on a budget this loop must drain.
                    let environment = SHELL_ENVIRONMENT_VARIABLES.lock().unwrap().clone();
                    if !incoming_permit.try_extend(incoming::environment_size(environment.as_ref())) {
                        let _ = response_tx.try_send(make_response(Response::Error(
                            "RunProcess busy: shell environment snapshot exceeds available incoming budget".into(),
                        )));
                        return Ok(());
                    }

                    tokio::spawn(async move {
                        let _incoming_permit = incoming_permit;
                        debug!("running command");

                        let timeout_duration = request.timeout.map_or(Duration::from_secs(60), Into::into);
                        let command_timeout = tokio::time::timeout(timeout_duration, async {
                            let _execution =
                                acquire_process_slot(incoming::PROCESS_SLOTS.clone(), &response_tx).await?;
                            // Run at the task's actual first poll, immediately
                            // before output starts. Once admitted to start,
                            // retain the existing command timeout on disconnect.
                            if !response_tx.is_current_ready() {
                                return None;
                            }
                            let mut cmd = create_command(&request.executable, directory, environment.as_ref());
                            // Command owns its copied environment now; the
                            // frozen Arc need not pin an old shell epoch too.
                            drop(environment);
                            cmd.args(request.arguments);
                            for var in request.env {
                                match var.value {
                                    Some(value) => cmd.env(var.key, value),
                                    None => cmd.env_remove(var.key),
                                };
                            }
                            Some(fastab_term::process_output::run(&mut cmd).await)
                        });

                        let response = match command_timeout.await {
                            Ok(None) => return,
                            Ok(Some(Ok(output))) => {
                                debug!("command successfully ran");
                                make_response(Response::RunProcess(output))
                            },
                            Ok(Some(Err(err))) => {
                                warn!(%err, executable = request.executable, "failed running executable");
                                make_response(Response::Error(format!(
                                    "failed running executable ({}): {err}",
                                    request.executable
                                )))
                            },
                            Err(err) => {
                                warn!(
                                    %err,
                                    executable = request.executable,
                                    timeout_ms =% timeout_duration.as_millis(),
                                    "timed out running executable"
                                );
                                make_response(Response::Error(format!(
                                    "timed out after {}ms running executable ({}): {err}",
                                    timeout_duration.as_millis(),
                                    request.executable,
                                )))
                            },
                        };

                        let _ = response_tx.try_send(response);
                    });
                },
                _ => warn!("unhandled request {request:?}"),
            }
        },
        Some(clientbound::Packet::Ping(())) => {
            let response = Hostbound {
                packet: Some(hostbound::Packet::Pong(())),
            };

            let _ = response_tx.try_send(response);
        },
        packet => warn!("unhandled packet {packet:?}"),
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn incoming_insert_writes_every_byte_across_real_socket_short_writes() {
        use alacritty_terminal::term::SizeInfo;
        use fastab_proto::figterm::InsertTextRequest;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        struct ShortWriter(UnixStream);

        #[async_trait::async_trait]
        impl AsyncMasterPty for ShortWriter {
            async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf).await
            }

            async fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.write(&buf[..buf.len().min(257)]).await
            }

            fn resize(&self, _: portable_pty::PtySize) -> Result<()> {
                Ok(())
            }
        }

        let (server, mut client) = UnixStream::pair().unwrap();
        let capture = tokio::spawn(async move {
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let (history, _history_rx) = flume::bounded(1);
        let term = Term::new(
            SizeInfo::new(24, 80),
            EventHandler::new(
                crate::ipc::RemoteSender::test_disconnected(),
                history,
                crate::event_handler::LocalEvents::default(),
                false,
            ),
            1,
            "short-insert-fixture".into(),
        );
        let mut master: Box<dyn AsyncMasterPty + Send + Sync> = Box::new(ShortWriter(server));
        let (control, _controls) = flume::bounded(16);
        let expected = "x".repeat(128 * 1024);
        let request = FigtermRequest::InsertText(InsertTextRequest {
            insertion: Some(expected.clone()),
            ..Default::default()
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            process_figterm_request(
                request,
                control,
                &term,
                &mut master,
                &mut KeyInterceptor::new(),
                RequestOrigin::Local,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        drop(master);
        assert_eq!(capture.await.unwrap(), expected.as_bytes());
    }

    #[tokio::test]
    async fn unstarted_run_process_retires_with_its_generation_while_waiting_for_a_slot() {
        let sender = crate::ipc::RemoteSender::test_ready();
        let generation = sender.phase().ready_generation().unwrap();
        let response = sender.for_generation(generation);
        let slots = Arc::new(Semaphore::new(1));
        let held = slots.clone().acquire_owned().await.unwrap();
        let waiting = tokio::spawn(async move { acquire_process_slot(slots, &response).await });
        tokio::task::yield_now().await;
        sender.test_retire();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        drop(held);
    }

    #[tokio::test]
    async fn test_create_command() {
        let output = create_command("cargo", "/", &[]).output().await.unwrap();
        assert!(output.status.success());

        let output = create_command("echo", "/", &[])
            .arg("hello world")
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello world\n");
        assert_eq!(output.stderr, b"");

        let output = create_command("bash", "/", &[])
            .args(["-c", "echo hello world 1 1>&2; echo hello world 2; exit 25"])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(25));
        assert_eq!(output.stdout, b"hello world 2\n");
        assert_eq!(output.stderr, b"hello world 1\n");
    }
}
