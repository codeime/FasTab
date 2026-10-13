pub(crate) mod commands;
mod hooks;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use fastab_ipc::{BufferedUnixStream, RecvMessage, SendMessage};
use fastab_os_shim::{Context as FigContext, ContextArcProvider, ContextProvider};
use fastab_proto::local::command_response::Response as CommandResponseTypes;
use fastab_proto::local::local_message::Type as LocalMessageType;
use fastab_proto::local::{CommandResponse, ErrorResponse, LocalMessage, SuccessResponse};
use fastab_remote_ipc::figterm::FigtermState;
use fastab_settings::settings::SettingsProvider;
use fastab_settings::{Settings, State, StateProvider};
use fastab_util::directories;
use tokio::net::UnixListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, trace, warn};

use crate::event::Event;
use crate::platform::PlatformState;
use crate::webview::notification::WebviewNotificationsState;
use crate::{AUTOCOMPLETE_ID, DASHBOARD_ID, EventLoopProxy};

const MAX_LOCAL_CONNECTIONS: usize = 64;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn active_connections() -> usize {
    ACTIVE_CONNECTIONS.load(Ordering::Relaxed)
}

struct ConnectionAdmission {
    _permit: OwnedSemaphorePermit,
}

impl Drop for ConnectionAdmission {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub enum LocalResponse {
    Error { code: Option<i32>, message: Option<String> },
    Success(Option<String>),
    Message(Box<CommandResponseTypes>),
}

pub type LocalResult = Result<LocalResponse, LocalResponse>;

#[derive(Debug)]
struct LocalIpcContext {
    settings: Settings,
    state: State,
    context: Arc<FigContext>,
}

impl LocalIpcContext {
    fn new() -> Self {
        Self {
            settings: Settings::new(),
            state: State::new(),
            context: FigContext::new(),
        }
    }
}

impl SettingsProvider for LocalIpcContext {
    fn settings(&self) -> &Settings {
        &self.settings
    }
}

impl StateProvider for LocalIpcContext {
    fn state(&self) -> &State {
        &self.state
    }
}

impl ContextProvider for LocalIpcContext {
    fn context(&self) -> &FigContext {
        &self.context
    }
}

impl ContextArcProvider for LocalIpcContext {
    fn context_arc(&self) -> Arc<FigContext> {
        Arc::clone(&self.context)
    }
}

pub async fn start_local_ipc(
    platform_state: Arc<PlatformState>,
    figterm_state: Arc<FigtermState>,
    webview_notifications_state: Arc<WebviewNotificationsState>,
    proxy: EventLoopProxy,
) -> Result<()> {
    let socket_path = directories::desktop_socket_path()?;
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

    let listener = UnixListener::bind(&socket_path)?;

    let slots = Arc::new(Semaphore::new(MAX_LOCAL_CONNECTIONS));
    while let Ok(permit) = slots.clone().acquire_owned().await {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        let admission = ConnectionAdmission { _permit: permit };
        let platform = platform_state.clone();
        let figterm = figterm_state.clone();
        let notifications = webview_notifications_state.clone();
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let _admission = admission;
            handle_local_ipc(
                BufferedUnixStream::new(stream),
                platform,
                figterm,
                notifications,
                proxy,
                LocalIpcContext::new(),
            )
            .await;
        });
    }

    Ok(())
}

async fn handle_local_ipc<Ctx>(
    mut stream: BufferedUnixStream,
    platform_state: Arc<PlatformState>,
    figterm_state: Arc<FigtermState>,
    webview_notifications_state: Arc<WebviewNotificationsState>,
    proxy: EventLoopProxy,
    ctx: Ctx,
) where
    Ctx: SettingsProvider + StateProvider + ContextProvider + ContextArcProvider + Send + Sync,
{
    let mut first_frame = true;
    loop {
        let received = if first_frame {
            // Covers both a silent peer and an incomplete first frame. Once
            // established, preserve the idle policy of existing IPC clients.
            match tokio::time::timeout(FIRST_FRAME_TIMEOUT, stream.recv_message::<LocalMessage>()).await {
                Ok(received) => received,
                Err(_) => {
                    debug!("Local IPC first frame timed out");
                    break;
                },
            }
        } else {
            stream.recv_message::<LocalMessage>().await
        };
        first_frame = false;
        let message = match received {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(err) => {
                if !err.is_disconnect() {
                    error!("Failed receiving local message: {err}");
                }
                break;
            },
        };
        trace!("Received local message: {message:?}");
        match message.r#type {
            Some(LocalMessageType::Command(command)) => {
                let response = match command.command {
                    None => LocalResponse::Error {
                        code: None,
                        message: Some("Local ipc command was None".into()),
                    },
                    Some(command) => {
                        use fastab_proto::local::command::Command::{
                            BundleMetadata, ConnectToIbus, DebugMode, Devtools, Diagnostics, DumpState, InputMethod,
                            ListTerminalIntegrations, LogLevel, Login, Logout, OpenBrowser, OpenUiElement,
                            PromptAccessibility, Quit, ReportWindow, ResetCache, Restart, RestartSettingsListener,
                            RunInstallScript, TerminalIntegration, Update,
                        };

                        match command {
                            DebugMode(command) => commands::debug(command, &proxy).await,
                            OpenUiElement(command) => commands::open_ui_element(command, &proxy).await,
                            Quit(command) => commands::quit(command, &proxy).await,
                            Diagnostics(command) => commands::diagnostic(command, &figterm_state).await,
                            OpenBrowser(command) => commands::open_browser(command).await,
                            PromptAccessibility(_) => commands::prompt_for_accessibility_permission(&ctx).await,
                            LogLevel(command) => commands::log_level(command),
                            Login(_) => commands::login(&proxy).await,
                            Logout(_) => commands::logout(&proxy).await,
                            DumpState(command) => {
                                commands::dump_state(
                                    command,
                                    &figterm_state,
                                    &webview_notifications_state,
                                    &platform_state,
                                    &proxy,
                                )
                                .await
                            },
                            ConnectToIbus(_) => commands::connect_to_ibus(proxy.clone(), &platform_state).await,
                            BundleMetadata(_) => commands::bundle_metadata(&ctx.context_arc()).await,
                            Update(_) => {
                                if crate::update::check_for_update(true, false).await {
                                    Ok(LocalResponse::Success(None))
                                } else {
                                    Err(LocalResponse::Error {
                                        code: None,
                                        message: Some("The Sparkle updater is unavailable".to_owned()),
                                    })
                                }
                            },
                            Devtools(command) => {
                                let window_id = match command.window() {
                                    fastab_proto::local::devtools_command::Window::DevtoolsAutocomplete => {
                                        AUTOCOMPLETE_ID
                                    },
                                    fastab_proto::local::devtools_command::Window::DevtoolsDashboard => DASHBOARD_ID,
                                };

                                proxy
                                    .send_event_async(Event::WindowEvent {
                                        window_id,
                                        window_event: crate::event::WindowEvent::Devtools,
                                    })
                                    .await
                                    .ok();

                                Ok(LocalResponse::Success(None))
                            },
                            TerminalIntegration(_)
                            | ListTerminalIntegrations(_)
                            | Restart(_)
                            | ReportWindow(_)
                            | RestartSettingsListener(_)
                            | RunInstallScript(_)
                            | ResetCache(_)
                            | InputMethod(_) => {
                                debug!(?command, "Unhandled command");
                                Err(LocalResponse::Error {
                                    code: None,
                                    message: Some("Unknown command".to_owned()),
                                })
                            },
                        }
                        .unwrap_or_else(|r| r)
                    },
                };

                match command.no_response {
                    Some(true) => {},
                    _ => {
                        let message = {
                            CommandResponse {
                                id: command.id,
                                response: Some(match response {
                                    LocalResponse::Error {
                                        code: exit_code,
                                        message,
                                    } => CommandResponseTypes::Error(ErrorResponse { exit_code, message }),
                                    LocalResponse::Success(message) => {
                                        CommandResponseTypes::Success(SuccessResponse { message })
                                    },
                                    LocalResponse::Message(m) => *m,
                                }),
                            }
                        };

                        match tokio::time::timeout(Duration::from_secs(5), stream.send_message(message)).await {
                            Ok(Ok(())) => {},
                            result => {
                                error!(?result, "Failed sending local response; retiring connection");
                                // The command might already have executed, or
                                // its reply might be partially written. Close
                                // this stream; never replay either on a new one.
                                break;
                            },
                        }
                    },
                }
            },
            Some(LocalMessageType::Hook(hook)) => {
                use fastab_proto::ReflectMessage;
                use fastab_proto::local::hook::Hook::{
                    Callback, CaretPosition, ClearAutocompleteCache, EditBuffer, Event, FileChanged, FocusChange,
                    FocusedWindowData, Hide, Init, IntegrationReady, InterceptedKey, KeyboardFocusChanged,
                    OpenedSshConnection, PostExec, PreExec, Prompt, TmuxPaneChanged,
                };

                if let Err(err) = match hook.hook {
                    Some(CaretPosition(request)) => hooks::caret_position(request, &proxy).await,
                    Some(FocusChange(_)) => hooks::focus_change(&proxy).await,
                    Some(FileChanged(request)) => hooks::file_changed(request).await,
                    Some(FocusedWindowData(request)) => {
                        hooks::focused_window_data(request, &platform_state, &proxy).await
                    },
                    Some(KeyboardFocusChanged(_)) => hooks::focus_change(&proxy).await,
                    Some(Event(event)) => hooks::event(event, &proxy).await,
                    Some(ClearAutocompleteCache(event)) => hooks::clear_autocomplete_cache(event, &proxy).await,
                    Some(
                        Init(_)
                        | PostExec(_)
                        | TmuxPaneChanged(_)
                        | OpenedSshConnection(_)
                        | Callback(_)
                        | IntegrationReady(_)
                        | Hide(_)
                        | PreExec(_)
                        | InterceptedKey(_)
                        | EditBuffer(_)
                        | Prompt(_),
                    ) => {
                        warn!("received legacy hook `{}`", hook.descriptor().name());
                        Ok(())
                    },
                    None => {
                        warn!("Received unknown or empty hook");
                        Ok(())
                    },
                } {
                    error!("Error processing hook: {err:?}");
                }
            },
            None => warn!("Received empty local message"),
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn silent_and_partial_first_frames_do_not_keep_connections_forever() {
        let (proxy, _events) = crate::event_loop::channel();
        let platform = Arc::new(PlatformState::new(proxy.clone()));
        let figterm = Arc::new(FigtermState::new());
        let notifications = Arc::new(WebviewNotificationsState::default());
        let mut clients = Vec::new();
        let mut servers = tokio::task::JoinSet::new();
        for partial in [false, true] {
            let (mut client, stream) = tokio::net::UnixStream::pair().unwrap();
            if partial {
                client.write_all(b"\x1b").await.unwrap();
            }
            clients.push(client);
            servers.spawn(handle_local_ipc(
                BufferedUnixStream::new(stream),
                platform.clone(),
                figterm.clone(),
                notifications.clone(),
                proxy.clone(),
                LocalIpcContext::new(),
            ));
        }
        tokio::time::timeout(FIRST_FRAME_TIMEOUT + Duration::from_secs(1), async {
            while let Some(server) = servers.join_next().await {
                server.unwrap();
            }
        })
        .await
        .expect("first-frame deadline must retire live silent/partial peers");
        // Peers stayed alive throughout: EOF cannot explain retirement.
        drop(clients);
    }
}
