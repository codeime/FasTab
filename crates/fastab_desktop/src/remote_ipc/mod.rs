use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fastab_proto::local::{EditBufferHook, InterceptedKeyHook, PostExecHook, PreExecHook, PromptHook};
use fastab_proto::remote::clientbound;
use fastab_remote_ipc::figterm::{FigtermState, SessionMetrics};
use time::OffsetDateTime;
use tracing::debug;
use uuid::Uuid;

use crate::EventLoopProxy;
use crate::event::Event;

#[derive(Debug, Clone)]
pub struct RemoteHook {
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
        // Stamp the input before the bounded overlay send can wait. A later
        // accepted native focus must still supersede this input.
        let input_sequence = self.proxy.begin_overlay_input();
        let mut redraw = false;
        let _old_metrics = figterm_state.with_update(session_id, |session| {
            redraw = session.edit_buffer.text == hook.text && session.edit_buffer.cursor == hook.cursor;
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
            .send_overlay_input_with_sequence(
                crate::event_loop::OverlayInput {
                    buffer: hook.text.clone(),
                    cwd,
                    cursor: hook.cursor.max(0) as u32,
                    session_id,
                    redraw,
                },
                input_sequence,
            )
            .await?;

        Ok(None)
    }

    async fn prompt(
        &mut self,
        hook: &PromptHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        figterm_state.with(&session_id, |session| {
            session.edit_buffer.end_input();
            session.apply_context(hook.context.clone());
        });

        let _ = self.proxy.send_event_async(Event::JevContextChanged).await;

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

        Ok(None)
    }

    async fn intercepted_key(
        &mut self,
        InterceptedKeyHook {
            action,
            key,
            context: _,
        }: InterceptedKeyHook,
        _session_id: Uuid,
    ) -> Result<Option<clientbound::response::Response>> {
        debug!(%action, "Intercepted Key Action");

        self.proxy
            .send_event_async(Event::AutocompleteAction {
                action,
                session_id: _session_id,
                key: Some(key).filter(|key| !key.is_empty()),
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
        let hook = RemoteHook { proxy };
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
                assert!(!input.redraw);
                state.with(&session_id, |session| {
                    assert_eq!(session.edit_buffer.text, text);
                    if text.len() < 64 * 1024 {
                        assert!(session.edit_buffer.text.capacity() < 64 * 1024);
                    }
                });
            }

            // Reporting the same line and cursor again is the shell redrawing it.
            for redraw in [false, true] {
                send_hook(&mut client, edit_request("tool ")).await;
                let Event::GpuiOverlayInput(queued) = next_event(&events).await else {
                    panic!("expected queued input");
                };
                assert_eq!(queued.take().unwrap().redraw, redraw);
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
