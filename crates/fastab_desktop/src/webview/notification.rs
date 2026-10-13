use anyhow::Result;
use base64::prelude::*;
use bytes::BytesMut;
use dashmap::DashMap;
use fastab_proto::fig::server_originated_message::Submessage as ServerOriginatedSubMessage;
use fastab_proto::fig::{Notification, NotificationType, ServerOriginatedMessage};
use fastab_proto::prost::Message;
use fnv::FnvBuildHasher;
use tracing::debug;

use crate::EventLoopProxy;
use crate::event::{EmitEventName, Event, WindowEvent};
use crate::webview::WindowId;

#[derive(Debug, Default)]
pub struct WebviewNotificationWindowState(pub DashMap<NotificationType, i64, FnvBuildHasher>);

impl std::ops::Deref for WebviewNotificationWindowState {
    type Target = DashMap<NotificationType, i64, FnvBuildHasher>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Default)]
pub struct WebviewNotificationsState {
    pub subscriptions: DashMap<WindowId, WebviewNotificationWindowState, FnvBuildHasher>,
}

impl WebviewNotificationsState {
    pub async fn broadcast_notification_all(
        &self,
        notification_type: &NotificationType,
        notification: Notification,
        proxy: &EventLoopProxy,
    ) -> Result<()> {
        // Legacy native callers may spawn a broadcast per callback. Keep
        // those on the explicitly diagnosed synchronous lane; awaiting here
        // would relocate an unbounded source into suspended sender tasks.
        self.broadcast_notification(notification_type, notification, proxy, false)
            .await
    }

    pub async fn broadcast_notification_all_bounded(
        &self,
        notification_type: &NotificationType,
        notification: Notification,
        proxy: &EventLoopProxy,
    ) -> Result<()> {
        self.broadcast_notification(notification_type, notification, proxy, true)
            .await
    }

    async fn broadcast_notification(
        &self,
        notification_type: &NotificationType,
        notification: Notification,
        proxy: &EventLoopProxy,
        bounded: bool,
    ) -> Result<()> {
        if self.subscriptions.is_empty() {
            return Ok(());
        }

        debug!(?notification_type, "Broadcasting webview notification");

        let subscribers: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|sub| sub.get(notification_type).map(|id| (sub.key().clone(), *id)))
            .collect();
        for (window_id, message_id) in subscribers {
            let message = ServerOriginatedMessage {
                id: Some(message_id),
                submessage: Some(ServerOriginatedSubMessage::Notification(notification.clone())),
            };

            let mut encoded = BytesMut::new();
            message.encode(&mut encoded)?;

            let event = Event::WindowEvent {
                window_id,
                window_event: WindowEvent::Emit {
                    event_name: EmitEventName::Notification,
                    payload: BASE64_STANDARD.encode(encoded).into(),
                },
            };
            if bounded {
                proxy.send_event_async(event).await?;
            } else {
                proxy.send_event(event)?;
            }
        }

        Ok(())
    }
}

impl serde::Serialize for WebviewNotificationWindowState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for subscription in self.0.iter() {
            map.serialize_entry(subscription.key(), &subscription.value())?;
        }
        map.end()
    }
}

impl serde::Serialize for WebviewNotificationsState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.subscriptions.len()))?;
        for subscription in self.subscriptions.iter() {
            map.serialize_entry(subscription.key(), &subscription.value())?;
        }
        map.end()
    }
}
