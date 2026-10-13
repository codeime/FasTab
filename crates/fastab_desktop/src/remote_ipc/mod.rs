use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::prelude::*;
use bytes::BytesMut;
use fastab_proto::fig::server_originated_message::Submessage as ServerOriginatedSubMessage;
use fastab_proto::fig::{
    EditBufferChangedNotification, HistoryUpdatedNotification, LocationChangedNotification, Notification,
    NotificationType, Process, ProcessChangedNotification, ServerOriginatedMessage, ShellPromptReturnedNotification,
};
use fastab_proto::local::{EditBufferHook, InterceptedKeyHook, PostExecHook, PreExecHook, PromptHook};
use fastab_proto::prost::Message;
use fastab_proto::remote::clientbound;
use fastab_remote_ipc::figterm::{FigtermState, SessionMetrics};
use time::OffsetDateTime;
use tracing::{debug, error};
use uuid::Uuid;

use crate::EventLoopProxy;
use crate::event::{EmitEventName, Event, WindowEvent};
use crate::webview::notification::WebviewNotificationsState;

#[derive(Debug, Clone)]
pub struct RemoteHook {
    pub notifications_state: Arc<WebviewNotificationsState>,
    pub proxy: EventLoopProxy,
}

#[async_trait::async_trait]
impl fastab_remote_ipc::RemoteHookHandler for RemoteHook {
    type Error = anyhow::Error;

    async fn sessions_changed(&mut self, _figterm_state: &Arc<FigtermState>) {
        let _ = self.proxy.send_event_async(Event::JevContextChanged).await;
    }

    async fn session_closed(&mut self, session_id: Uuid) {
        let _ = self
            .proxy
            .send_event_async(Event::GpuiOverlayEndInput { session_id })
            .await;
    }

    async fn edit_buffer(
        &mut self,
        hook: &EditBufferHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        let _old_metrics = figterm_state.with_update(session_id, |session| {
            session.edit_buffer.update(&hook.text, hook.cursor);
            session
                .terminal_cursor_coordinates
                .clone_from(&hook.terminal_cursor_coordinates);
            session.apply_context(hook.context.clone());

            let received_at = OffsetDateTime::now_utc();
            let current_session_expired = session
                .current_session_metrics
                .as_ref()
                .is_some_and(|metrics| received_at > metrics.end_time + Duration::from_secs(5));

            if current_session_expired {
                let previous = session.current_session_metrics.clone();
                session.current_session_metrics = Some(SessionMetrics::new(received_at));
                previous
            } else {
                if let Some(ref mut metrics) = session.current_session_metrics {
                    metrics.end_time = received_at;
                }
                None
            }
        });

        // GPUI never inserts here. Skip UTF-16 / protobuf / base64 unless a
        // leftover WebView subscriber is actually listening.
        if !self.notifications_state.subscriptions.is_empty() {
            let utf16_cursor_position = hook
                .text
                .get(..hook.cursor as usize)
                .map(|s| s.encode_utf16().count() as i32);

            // Do not retain a DashMap shard guard across bounded admission.
            let subscribers: Vec<_> = self
                .notifications_state
                .subscriptions
                .iter()
                .filter_map(|sub| {
                    sub.get(&NotificationType::NotifyOnEditbuffferChange)
                        .map(|id| (sub.key().clone(), *id))
                })
                .collect();
            for (window_id, message_id) in subscribers {
                let hook = hook.clone();
                let message = ServerOriginatedMessage {
                    id: Some(message_id),
                    submessage: Some(ServerOriginatedSubMessage::Notification(Notification {
                        r#type: Some(fastab_proto::fig::notification::Type::EditBufferNotification(
                            EditBufferChangedNotification {
                                context: hook.context,
                                buffer: Some(hook.text),
                                cursor: utf16_cursor_position,
                                session_id: Some(session_id.into()),
                            },
                        )),
                    })),
                };

                let mut encoded = BytesMut::new();
                message.encode(&mut encoded).unwrap();

                debug!(%message_id, "Sending edit buffer change notification to webview");

                self.proxy
                    .send_event_async(Event::WindowEvent {
                        window_id,
                        window_event: WindowEvent::Emit {
                            event_name: EmitEventName::Notification,
                            payload: BASE64_STANDARD.encode(encoded).into(),
                        },
                    })
                    .await?;
            }
        }

        let cwd = figterm_state
            .with(&session_id, |session| {
                session
                    .context
                    .as_ref()
                    .and_then(|ctx| ctx.current_working_directory.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        self.proxy
            .send_overlay_input_async(crate::event_loop::OverlayInput {
                buffer: hook.text.clone(),
                cwd,
                cursor: hook.cursor.max(0) as u32,
                session_id,
            })
            .await?;

        Ok(None)
    }

    async fn prompt(
        &mut self,
        hook: &PromptHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        let mut cwd_changed = false;
        let mut new_cwd = None;
        figterm_state.with(&session_id, |session| {
            session.edit_buffer.end_input();
            if let (Some(old_context), Some(new_context)) = (&session.context, &hook.context) {
                cwd_changed = old_context.current_working_directory != new_context.current_working_directory;
                new_cwd.clone_from(&new_context.current_working_directory);
            }

            session.apply_context(hook.context.clone());
        });

        let _ = self.proxy.send_event_async(Event::JevContextChanged).await;

        if cwd_changed {
            if let Err(err) = self
                .notifications_state
                .broadcast_notification_all_bounded(
                    &NotificationType::NotifyOnLocationChange,
                    Notification {
                        r#type: Some(fastab_proto::fig::notification::Type::LocationChangedNotification(
                            LocationChangedNotification {
                                session_id: Some(session_id.to_string()),
                                host_name: hook.context.as_ref().and_then(|ctx| ctx.hostname.clone()),
                                user_name: None,
                                directory: new_cwd,
                            },
                        )),
                    },
                    &self.proxy,
                )
                .await
            {
                error!(%err, "Failed to broadcast LocationChangedNotification");
            }
        }

        if let Err(err) = self
            .notifications_state
            .broadcast_notification_all_bounded(
                &NotificationType::NotifyOnPrompt,
                Notification {
                    r#type: Some(fastab_proto::fig::notification::Type::ShellPromptReturnedNotification(
                        ShellPromptReturnedNotification {
                            session_id: Some(session_id.to_string()),
                            shell: hook.context.as_ref().map(|ctx| Process {
                                pid: ctx.pid,
                                executable: ctx.process_name.clone(),
                                directory: ctx.current_working_directory.clone(),
                                env: vec![],
                            }),
                        },
                    )),
                },
                &self.proxy,
            )
            .await
        {
            error!(%err, "Failed to broadcast ShellPromptReturnedNotification");
        }

        Ok(None)
    }

    async fn pre_exec(
        &mut self,
        hook: &PreExecHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        figterm_state.with_update(session_id, |session| {
            session.edit_buffer.end_input();
            session.apply_context(hook.context.clone());
        });

        self.proxy
            .send_event_async(Event::GpuiOverlayEndInput { session_id })
            .await?;

        self.notifications_state
            .broadcast_notification_all_bounded(
                &NotificationType::NotifyOnProcessChanged,
                Notification {
                    r#type: Some(fastab_proto::fig::notification::Type::ProcessChangeNotification(
                        ProcessChangedNotification {
                        session_id: Some(session_id.to_string()),
                        new_process: // TODO: determine active application based on tty
                        hook.context.as_ref().map(|ctx| Process {
                            pid: ctx.pid,
                            executable: ctx.process_name.clone(),
                            directory: ctx.current_working_directory.clone(),
                            env: vec![],
                        }),
                    },
                    )),
                },
                &self.proxy,
            )
            .await?;

        Ok(None)
    }

    async fn post_exec(
        &mut self,
        hook: &PostExecHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        figterm_state.with_update(session_id, |session| {
            session.apply_context(hook.context.clone());
        });

        let _ = self.proxy.send_event_async(Event::JevContextChanged).await;

        self.notifications_state
            .broadcast_notification_all_bounded(
                &NotificationType::NotifyOnHistoryUpdated,
                Notification {
                    r#type: Some(fastab_proto::fig::notification::Type::HistoryUpdatedNotification(
                        HistoryUpdatedNotification {
                            command: hook.command.clone(),
                            process_name: hook.context.as_ref().and_then(|ctx| ctx.process_name.clone()),
                            current_working_directory: hook
                                .context
                                .as_ref()
                                .and_then(|ctx| ctx.current_working_directory.clone()),
                            session_id: Some(session_id.to_string()),
                            hostname: hook.context.as_ref().and_then(|ctx| ctx.hostname.clone()),
                            exit_code: hook.exit_code,
                        },
                    )),
                },
                &self.proxy,
            )
            .await?;

        Ok(None)
    }

    async fn intercepted_key(
        &mut self,
        InterceptedKeyHook { action, context: _, .. }: InterceptedKeyHook,
        _session_id: Uuid,
    ) -> Result<Option<clientbound::response::Response>> {
        debug!(%action, "Intercepted Key Action");

        self.proxy
            .send_event_async(Event::AutocompleteAction {
                action,
                session_id: _session_id,
            })
            .await?;

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastab_ipc::SendMessage;
    use fastab_proto::local::ShellContext;
    use fastab_proto::remote::{Hostbound, hostbound};
    use tokio::net::UnixStream;

    async fn next_event(events: &crate::event_loop::EventLoopReceiver) -> Event {
        tokio::time::timeout(Duration::from_secs(3), events.recv_async())
            .await
            .expect("desktop event deadline")
            .expect("desktop event")
    }

    async fn send_hook(client: &mut UnixStream, request: hostbound::request::Request) {
        client
            .send_message(Hostbound {
                packet: Some(hostbound::Packet::Request(hostbound::Request {
                    request: Some(request),
                    nonce: None,
                })),
            })
            .await
            .unwrap();
    }

    fn edit_request(text: &str) -> hostbound::request::Request {
        hostbound::request::Request::EditBuffer(EditBufferHook {
            context: Some(ShellContext {
                current_working_directory: Some("/fixture".into()),
                ..Default::default()
            }),
            text: text.into(),
            cursor: text.len() as i64,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn edit_buffers_reach_overlay_before_caret_refresh() {
        let state = Arc::new(FigtermState::new());
        let (proxy, events) = crate::event_loop::channel();
        let hook = RemoteHook {
            notifications_state: Arc::new(WebviewNotificationsState::default()),
            proxy,
        };
        let mut session_ids = Vec::new();
        // Real handshakes exercise distinct session owners even for the same
        // shell ID, followed by the production hook and desktop event queue.
        for empty_buffer in ["", " \t "] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let server = tokio::spawn(fastab_remote_ipc::remote::handle_remote_ipc(
                server,
                state.clone(),
                hook.clone(),
            ));
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
            assert!(matches!(next_event(&events).await, Event::JevContextChanged));
            let session_id = state.inner.lock().most_recent.unwrap();
            assert!(!session_ids.contains(&session_id));
            session_ids.push(session_id);

            let large = "x".repeat(128 * 1024);
            for text in [large.as_str(), "tool ", empty_buffer] {
                send_hook(&mut client, edit_request(text)).await;
                let Event::GpuiOverlayInput(queued) = next_event(&events).await else {
                    panic!("expected queued input");
                };
                let input = queued.take().unwrap();
                assert_eq!(input.buffer, text);
                assert_eq!(input.cwd, "/fixture");
                assert_eq!(input.cursor, text.len() as u32);
                assert_eq!(input.session_id, session_id);
                state.with(&session_id, |session| {
                    assert_eq!(session.edit_buffer.text, text);
                    if text.len() < 64 * 1024 {
                        assert!(session.edit_buffer.text.capacity() < 64 * 1024);
                    }
                });
            }

            // End-input hooks must release a large retained command even when
            // the shell does not send an intervening empty EditBufferHook.
            for request in [
                hostbound::request::Request::Prompt(PromptHook::default()),
                hostbound::request::Request::PreExec(PreExecHook::default()),
            ] {
                send_hook(&mut client, edit_request(&large)).await;
                assert!(matches!(next_event(&events).await, Event::GpuiOverlayInput(_)));
                let is_prompt = matches!(request, hostbound::request::Request::Prompt(_));
                send_hook(&mut client, request).await;
                let event = next_event(&events).await;
                if is_prompt {
                    assert!(matches!(event, Event::JevContextChanged));
                } else {
                    assert!(matches!(event, Event::GpuiOverlayEndInput { session_id: owner } if owner == session_id));
                }
                state.with(&session_id, |session| {
                    assert!(session.edit_buffer.text.is_empty());
                    assert_eq!(session.edit_buffer.text.capacity(), 0);
                    assert_eq!(session.edit_buffer.cursor, 0);
                });
            }

            drop(client);
            tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .expect("connection cleanup deadline")
                .unwrap();
            // Empty/whitespace buffers must not request another caret. The
            // next queued event is the connection's authenticated end-input.
            assert!(matches!(
                next_event(&events).await,
                Event::GpuiOverlayEndInput { session_id: owner } if owner == session_id
            ));
            assert!(matches!(next_event(&events).await, Event::JevContextChanged));
            assert!(events.is_empty());
        }
    }
}
