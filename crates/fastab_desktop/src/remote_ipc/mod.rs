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
        let _ = self.proxy.send_event(Event::JevContextChanged);
    }

    async fn session_closed(&mut self, session_id: Uuid) {
        let _ = self.proxy.send_event(Event::GpuiOverlayEndInput { session_id });
    }

    async fn edit_buffer(
        &mut self,
        hook: &EditBufferHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        let _old_metrics = figterm_state.with_update(session_id, |session| {
            session.edit_buffer.text.clone_from(&hook.text);
            session.edit_buffer.cursor.clone_from(&hook.cursor);
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

            for sub in self.notifications_state.subscriptions.iter() {
                let message_id = match sub.get(&NotificationType::NotifyOnEditbuffferChange) {
                    Some(id) => *id,
                    None => continue,
                };

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
                    .send_event(Event::WindowEvent {
                        window_id: sub.key().clone(),
                        window_event: WindowEvent::Emit {
                            event_name: EmitEventName::Notification,
                            payload: BASE64_STANDARD.encode(encoded).into(),
                        },
                    })
                    .unwrap();
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
        self.proxy.send_overlay_input(crate::event_loop::OverlayInput {
            buffer: hook.text.clone(),
            cwd,
            cursor: hook.cursor.max(0) as u32,
            session_id,
        })?;

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
            if let (Some(old_context), Some(new_context)) = (&session.context, &hook.context) {
                cwd_changed = old_context.current_working_directory != new_context.current_working_directory;
                new_cwd.clone_from(&new_context.current_working_directory);
            }

            session.apply_context(hook.context.clone());
        });

        let _ = self.proxy.send_event(Event::JevContextChanged);

        if cwd_changed {
            if let Err(err) = self
                .notifications_state
                .broadcast_notification_all(
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
            .broadcast_notification_all(
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
            session.apply_context(hook.context.clone());
        });

        self.proxy.send_event(Event::GpuiOverlayEndInput { session_id })?;

        self.notifications_state
            .broadcast_notification_all(
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

        let _ = self.proxy.send_event(Event::JevContextChanged);

        self.notifications_state
            .broadcast_notification_all(
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

        self.proxy.send_event(Event::AutocompleteAction {
            action,
            session_id: _session_id,
        })?;

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

    async fn next_event(events: &flume::Receiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(3), events.recv_async())
            .await
            .expect("desktop event deadline")
            .expect("desktop event")
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

            for text in ["tool ", empty_buffer] {
                client
                    .send_message(Hostbound {
                        packet: Some(hostbound::Packet::Request(hostbound::Request {
                            request: Some(hostbound::request::Request::EditBuffer(EditBufferHook {
                                context: Some(ShellContext {
                                    current_working_directory: Some("/fixture".into()),
                                    ..Default::default()
                                }),
                                text: text.into(),
                                cursor: text.len() as i64,
                                ..Default::default()
                            })),
                            nonce: None,
                        })),
                    })
                    .await
                    .unwrap();
                let Event::GpuiOverlayInput(queued) = next_event(&events).await else {
                    panic!("expected queued input");
                };
                let input = queued.take().unwrap();
                assert_eq!(input.buffer, text);
                assert_eq!(input.cwd, "/fixture");
                assert_eq!(input.cursor, text.len() as u32);
                assert_eq!(input.session_id, session_id);
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
