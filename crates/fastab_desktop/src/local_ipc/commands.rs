use std::sync::Mutex;
use std::time::Duration;

use fastab_os_shim::{Context, ContextArcProvider, ContextProvider};
use fastab_proto::local::command_response::Response as CommandResponseTypes;
use fastab_proto::local::dump_state_command::Type as DumpStateType;
use fastab_proto::local::{
    BundleMetadataResponse, DebugModeCommand, DiagnosticsCommand, DiagnosticsResponse, DumpStateCommand,
    DumpStateResponse, LogLevelCommand, LogLevelResponse, OpenBrowserCommand, OpenUiElementCommand, QuitCommand,
    UiElement,
};
use fastab_remote_ipc::figterm::FigtermState;
use fastab_settings::StateProvider;
use fastab_settings::settings::SettingsProvider;
use tao::event_loop::ControlFlow;
use tracing::{debug, error};

use super::{LocalResponse, LocalResult};
use crate::event::{Event, WindowEvent};
use crate::jev::diagnostics::{self, Metric};
use crate::platform::PlatformState;
use crate::webview::DASHBOARD_SIZE;
use crate::{AUTOCOMPLETE_ID, DASHBOARD_ID, EventLoopProxy, platform};

pub async fn debug(command: DebugModeCommand, proxy: &EventLoopProxy) -> LocalResult {
    static DEBUG_MODE: Mutex<bool> = Mutex::new(false);

    let debug_mode = match command.set_debug_mode {
        Some(b) => {
            *DEBUG_MODE.lock().unwrap() = b;
            b
        },
        None => match command.toggle_debug_mode {
            Some(true) => {
                let mut locked_debug = DEBUG_MODE.lock().unwrap();
                *locked_debug = !*locked_debug;
                *locked_debug
            },
            _ => *DEBUG_MODE.lock().unwrap(),
        },
    };

    proxy
        .send_event_async(Event::WindowEvent {
            window_id: AUTOCOMPLETE_ID.clone(),
            window_event: WindowEvent::DebugMode(debug_mode),
        })
        .await
        .map_err(event_admission_error)?;

    Ok(LocalResponse::Success(None))
}

pub async fn quit(_: QuitCommand, proxy: &EventLoopProxy) -> LocalResult {
    proxy
        .send_event_async(Event::ControlFlow(ControlFlow::Exit))
        .await
        .map(|_| LocalResponse::Success(None))
        .map_err(|_err| {
            #[allow(clippy::exit)]
            std::process::exit(0)
        })
}

pub async fn diagnostic(_: DiagnosticsCommand, figterm_state: &FigtermState) -> LocalResult {
    let ai = diagnostics::snapshot();
    debug!(
        evaluated = ai.evaluated,
        skipped = ai.skipped,
        skip_reasons = ?ai.skip_reasons,
        last_status = ?ai.last_status,
        eligible = ai.count(Metric::Eligible),
        requests = ai.count(Metric::Request),
        cache_hits = ai.count(Metric::CacheHit),
        responses = ai.count(Metric::Response),
        kept_local = ai.count(Metric::KeptLocal),
        promoted = ai.count(Metric::Promoted),
        accepted = ai.count(Metric::Accepted),
        not_accepted = ai.count(Metric::NotAccepted),
        cancelled = ai.count(Metric::Cancelled),
        failed = ai.count(Metric::Failed),
        latency_samples = ai.latency_samples,
        average_latency_ms = ?ai.average_latency_ms,
        p95_latency_ms = ?ai.p95_latency_ms,
        "AI aggregate diagnostics"
    );
    let (edit_buffer_string, edit_buffer_cursor, shell_context, intercept_enabled, intercept_global_enabled) = {
        match figterm_state.most_recent() {
            Some(session) => (
                Some(session.edit_buffer.text.clone()),
                Some(session.edit_buffer.cursor),
                session.context.clone(),
                Some(session.intercept.into()),
                Some(session.intercept_global.into()),
            ),
            None => (None, None, None, None, None),
        }
    };

    let response = DiagnosticsResponse {
        autocomplete_active: Some(platform::autocomplete_active()),
        #[cfg(target_os = "macos")]
        path_to_bundle: macos_utils::bundle::get_bundle_path()
            .and_then(|path| path.to_str().map(|s| s.to_owned()))
            .unwrap_or_default(),
        #[cfg(target_os = "macos")]
        accessibility: if macos_utils::accessibility::accessibility_is_enabled() {
            "true".into()
        } else {
            "false".into()
        },

        edit_buffer_string,
        edit_buffer_cursor,
        shell_context,
        intercept_enabled,
        intercept_global_enabled,

        ..Default::default()
    };

    Ok(LocalResponse::Message(Box::new(CommandResponseTypes::Diagnostics(
        response,
    ))))
}

pub async fn open_ui_element(command: OpenUiElementCommand, proxy: &EventLoopProxy) -> LocalResult {
    match command.element() {
        UiElement::Settings => {
            proxy
                .send_event_async(Event::WindowEvent {
                    window_id: DASHBOARD_ID.clone(),
                    window_event: WindowEvent::Batch(vec![
                        WindowEvent::NavigateRelative {
                            path: "/preferences".into(),
                        },
                        WindowEvent::Show,
                    ]),
                })
                .await
                .map_err(event_admission_error)?;
        },
        UiElement::MissionControl => {
            let events = if let Some(path) = command.route {
                vec![WindowEvent::NavigateRelative { path: path.into() }, WindowEvent::Show]
            } else {
                vec![WindowEvent::Show]
            };

            proxy
                .send_event_async(Event::WindowEvent {
                    window_id: DASHBOARD_ID.clone(),
                    window_event: WindowEvent::Batch(events),
                })
                .await
                .map_err(event_admission_error)?;
        },
        UiElement::MenuBar => error!("Opening menu bar is unimplemented"),
        UiElement::InputMethodPrompt => error!("Opening input method prompt is unimplemented"),
    };

    Ok(LocalResponse::Success(None))
}

pub async fn open_browser(command: OpenBrowserCommand) -> LocalResult {
    if let Err(err) = fastab_util::open_url(command.url) {
        error!(%err, "Error opening browser");
    }
    Ok(LocalResponse::Success(None))
}

#[allow(unused_variables)]
pub async fn prompt_for_accessibility_permission<Ctx>(ctx: &Ctx) -> LocalResult
where
    Ctx: SettingsProvider + StateProvider + ContextProvider + ContextArcProvider + Send + Sync,
{
    cfg_if::cfg_if! {
        if #[cfg(target_os = "macos")] {
            use fastab_desktop_api::requests::install::install;
            use fastab_proto::fig::{InstallRequest, InstallComponent, InstallAction};

            install(
                InstallRequest {
                    component: InstallComponent::Accessibility.into(),
                    action: InstallAction::Install.into()
                },
                ctx
            ).await.ok();
            Ok(LocalResponse::Success(None))
        } else {
            Err(LocalResponse::Error {
                code: None,
                message: Some("Accessibility API not supported on this platform".to_owned()),
            })
        }
    }
}

pub fn log_level(LogLevelCommand { level }: LogLevelCommand) -> LocalResult {
    let old_level = fastab_log::set_log_level(level).map_err(|err| LocalResponse::Error {
        code: None,
        message: Some(format!("Error setting log level: {err}")),
    })?;

    Ok(LocalResponse::Message(Box::new(CommandResponseTypes::LogLevel(
        LogLevelResponse {
            old_level: Some(old_level),
        },
    ))))
}

pub async fn login(proxy: &EventLoopProxy) -> LocalResult {
    proxy
        .send_event_async(Event::WindowEvent {
            window_id: DASHBOARD_ID,
            window_event: WindowEvent::Batch(vec![
                WindowEvent::UpdateWindowGeometry {
                    size: Some(DASHBOARD_SIZE),
                    position: None,
                    anchor: None,
                    tx: None,
                    dry_run: false,
                },
                WindowEvent::Reload,
                WindowEvent::Show,
            ]),
        })
        .await
        .map_err(event_admission_error)?;

    proxy
        .send_event_async(Event::ReloadTray { is_logged_in: true })
        .await
        .map_err(event_admission_error)?;

    Ok(LocalResponse::Success(None))
}

pub async fn logout(proxy: &EventLoopProxy) -> LocalResult {
    // fig_auth removed

    proxy
        .send_event_async(Event::WindowEvent {
            window_id: DASHBOARD_ID,
            window_event: WindowEvent::Batch(vec![WindowEvent::Reload, WindowEvent::Show]),
        })
        .await
        .map_err(event_admission_error)?;

    proxy
        .send_event_async(Event::ReloadTray { is_logged_in: true })
        .await
        .map_err(event_admission_error)?;

    Ok(LocalResponse::Success(None))
}

pub async fn dump_state(
    command: DumpStateCommand,
    figterm_state: &FigtermState,
    platform_state: &PlatformState,
    proxy: &EventLoopProxy,
) -> LocalResult {
    // Prost's enum accessor maps unknown values to zero (Figterm), which can
    // reveal shell state when a newer client asks for an unsupported component.
    let component = DumpStateType::try_from(command.r#type).map_err(|_invalid_component| LocalResponse::Error {
        code: None,
        message: Some("Unsupported dump-state component".to_owned()),
    })?;
    let json = match component {
        DumpStateType::DumpStateFigterm => {
            serde_json::to_string_pretty(&figterm_state).unwrap_or_else(|err| format!("unable to dump: {err}"))
        },
        // No WebView window subscribes any more; report the empty set this
        // component has always returned since the overlay went native.
        DumpStateType::DumpStateWebNotifications => "{}".to_owned(),
        DumpStateType::DumpStatePlatform => {
            serde_json::to_string_pretty(&platform_state).unwrap_or_else(|err| format!("unable to dump: {err}"))
        },
        DumpStateType::DumpStateEngine => {
            // Leave time for the existing CLI's two-second response timeout.
            engine_diagnostics_json(proxy, Duration::from_millis(1500)).await?
        },
    };

    LocalResult::Ok(LocalResponse::Message(Box::new(CommandResponseTypes::DumpState(
        DumpStateResponse { json },
    ))))
}

async fn engine_diagnostics_json(proxy: &EventLoopProxy, timeout: Duration) -> Result<String, LocalResponse> {
    let (reply, receiver) = futures::channel::oneshot::channel();
    let snapshot = tokio::time::timeout(timeout, async {
        proxy
            .send_event_async(Event::EngineDiagnostics { reply })
            .await
            .map_err(|_disconnected| engine_diagnostics_error("Desktop event loop is unavailable"))?;
        Ok::<_, LocalResponse>(receiver.await)
    })
    .await
    .map_err(|_timeout| engine_diagnostics_error("Engine resource diagnostics timed out"))??
    .map_err(|_cancelled| engine_diagnostics_error("Desktop dropped the engine resource diagnostics request"))?
    .map_err(|_engine_error| engine_diagnostics_error("Engine resource diagnostics are unavailable"))?;
    serde_json::to_string_pretty(&snapshot)
        .map_err(|_serialization_error| engine_diagnostics_error("Unable to serialize engine resource diagnostics"))
}

fn event_admission_error(error: crate::event_loop::EventAdmissionError) -> LocalResponse {
    LocalResponse::Error {
        code: None,
        message: Some(error.to_string()),
    }
}

fn engine_diagnostics_error(message: &str) -> LocalResponse {
    LocalResponse::Error {
        code: None,
        message: Some(message.to_owned()),
    }
}

#[allow(unused_variables)]
pub async fn connect_to_ibus(proxy: EventLoopProxy, platform_state: &PlatformState) -> LocalResult {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "linux")] {
            use crate::platform::ibus::launch_ibus_connection;
            match launch_ibus_connection(proxy, platform_state.inner()).await {
                Ok(_) => Ok(LocalResponse::Success(None)),
                Err(err) => {
                    Err(LocalResponse::Error {
                        code: None,
                        message: Some(format!("Failed connecting to ibus: {:?}", err)),
                    })
                },
            }
        } else {
            Err(LocalResponse::Error {
                code: None,
                message: Some("Connecting to IBus is only supported on Linux".to_owned()),
            })
        }
    }
}

pub async fn bundle_metadata(ctx: &Context) -> LocalResult {
    match fastab_util::manifest::bundle_metadata_json(ctx).await {
        Ok(json) => {
            // macOS has no packaged metadata. Still report the mapped process,
            // and preserve any packaged fields on platforms that provide them.
            let mut metadata = match json {
                Some(json) => {
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json).map_err(|error| {
                        LocalResponse::Error {
                            code: None,
                            message: Some(format!("Invalid bundled metadata: {error}")),
                        }
                    })?
                },
                None => serde_json::Map::new(),
            };
            metadata.insert(
                "running-process".into(),
                serde_json::from_str(&fastab_util::build_identity::json("fastab")).unwrap(),
            );
            Ok(LocalResponse::Message(Box::new(CommandResponseTypes::BundleMetadata(
                BundleMetadataResponse {
                    json: Some(serde_json::Value::Object(metadata).to_string()),
                },
            ))))
        },
        Err(err) => Err(LocalResponse::Error {
            code: None,
            message: Some(format!("Failed to get the bundled metadata: {err:?}")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use fastab_engine::{EngineClient, EngineClientDiagnostics};

    use super::*;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bundle_metadata_reports_the_running_process_without_packaged_metadata() {
        let context = Context::new();
        let Ok(LocalResponse::Message(response)) = bundle_metadata(&context).await else {
            panic!("expected running process metadata");
        };
        let CommandResponseTypes::BundleMetadata(response) = *response else {
            panic!("expected bundle metadata response");
        };
        let metadata: serde_json::Value = serde_json::from_str(&response.json.unwrap()).unwrap();
        assert_eq!(metadata["running-process"]["pid"], std::process::id());
        assert_eq!(metadata["running-process"]["component"], "fastab");
    }

    #[tokio::test]
    async fn engine_dump_reads_the_existing_worker_without_initializing_it() {
        let dir = tempfile::tempdir().expect("temporary specs directory");
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("lazy worker");
        let (proxy, events) = crate::event_loop::channel();
        let figterm = FigtermState::new();
        let platform = PlatformState::new(proxy.clone());
        let serve = async {
            let Event::EngineDiagnostics { reply } = events.recv_async().await.expect("diagnostic event") else {
                panic!("expected a read-only engine diagnostic request");
            };
            let _ = reply.send(engine.diagnostics().await);
        };
        let request = dump_state(
            DumpStateCommand {
                r#type: DumpStateType::DumpStateEngine.into(),
            },
            &figterm,
            &platform,
            &proxy,
        );
        let (response, ()) = tokio::join!(request, serve);
        let Ok(LocalResponse::Message(response)) = response else {
            panic!("expected a diagnostics response");
        };
        let CommandResponseTypes::DumpState(response) = *response else {
            panic!("expected dump-state response");
        };
        let snapshot: EngineClientDiagnostics = serde_json::from_str(&response.json).expect("numeric snapshot");
        assert_eq!(snapshot, EngineClientDiagnostics::default());
    }

    #[tokio::test]
    async fn unknown_dump_component_never_falls_back_to_shell_state() {
        let (proxy, events) = crate::event_loop::channel();
        let response = dump_state(
            DumpStateCommand { r#type: i32::MAX },
            &FigtermState::new(),
            &PlatformState::new(proxy.clone()),
            &proxy,
        )
        .await;
        assert!(matches!(response, Err(LocalResponse::Error { .. })));
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn engine_diagnostic_timeout_cancels_the_pending_reply() {
        let (proxy, events) = crate::event_loop::channel();
        let response = engine_diagnostics_json(&proxy, Duration::from_millis(10)).await;
        assert!(matches!(response, Err(LocalResponse::Error { .. })));
        let Event::EngineDiagnostics { reply } = events.try_recv().expect("pending diagnostic event") else {
            panic!("expected diagnostics");
        };
        assert!(reply.is_canceled());

        drop(events);
        assert!(
            engine_diagnostics_json(&proxy, Duration::from_millis(10))
                .await
                .is_err()
        );
    }
}
