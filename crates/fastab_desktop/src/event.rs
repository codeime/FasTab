use std::borrow::Cow;

use fastab_proto::local::caret_position_hook::Origin;
use tao::dpi::{LogicalSize, Position, Size};
use tao::event_loop::ControlFlow;
use tao::window::Theme;
use tokio::sync::mpsc::UnboundedSender;

use crate::platform::PlatformBoundEvent;
use crate::webview::WindowId;

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum Event {
    WindowEvent {
        window_id: WindowId,
        window_event: WindowEvent,
    },
    WindowEventAll {
        window_event: WindowEvent,
    },

    PlatformBoundEvent(PlatformBoundEvent),
    ControlFlow(ControlFlow),
    SetTrayVisible(bool),

    ReloadCredentials,
    GpuiOverlayVisibleRows {
        generation: u64,
        revision: (u64, u64),
        range: std::ops::Range<usize>,
    },
    JevSettingsChanged,
    JevCredentialsLoaded {
        epoch: u64,
        credentials: crate::overlay::ai::LoadedCredentials,
    },
    JevDebounced(crate::overlay::ai::RequestToken),
    JevComplete {
        token: crate::overlay::ai::RequestToken,
        result: crate::overlay::ai::Completed,
    },
    JevContextChanged,
    ReloadAccessibility,
    ReloadTray {
        is_logged_in: bool,
    },

    /// Menu bar or tray item activated. Delivered by `muda::MenuEvent::set_event_handler`.
    MenuClicked(String),
    /// Fresh permission check for the native settings gate.
    PermissionSnapshot(crate::permissions::PermissionSnapshot),

    /// Read-only resource counters from the existing completion worker.
    EngineDiagnostics {
        reply: futures::channel::oneshot::Sender<anyhow::Result<fastab_engine::EngineClientDiagnostics>>,
    },

    /// Headless engine input for the GPUI overlay.
    GpuiOverlayInput(std::sync::Arc<crate::event_loop::QueuedOverlayInput>),
    GpuiOverlayBuffer {
        buffer: String,
        cwd: String,
        cursor: u32,
        session_id: uuid::Uuid,
    },
    /// The shell executed its input. Background sessions must not end the
    /// active terminal's request or hide its overlay.
    GpuiOverlayEndInput {
        session_id: uuid::Uuid,
    },
    /// The current completion request exceeded the loading threshold. This is
    /// routed through the host event queue so GPUI state is only touched from
    /// the top-level dispatcher.
    GpuiOverlayLoading {
        generation: u64,
    },
    /// The request outlived the user's script budget. Only the `···` marker is
    /// retired; the request itself keeps running under the engine watchdog.
    GpuiOverlayLoadingExpired {
        generation: u64,
    },
    /// An explicit Show/Tab could not resolve before its bounded deadline.
    GpuiOverlayShowExpired {
        token: u64,
    },
    /// A completion request finished on the engine thread.
    GpuiOverlayComplete {
        generation: u64,
        result: anyhow::Result<fastab_engine::CompleteResult>,
        session_id: uuid::Uuid,
        cwd: String,
    },
    /// Generators were delayed by Fig `debounce`. Re-run the same buffer once
    /// the delay elapses, unless a newer generation has already replaced it.
    GpuiOverlayDebouncedComplete {
        generation: u64,
        /// The debounce that just elapsed, already spent from the `···`
        /// threshold.
        waited_ms: u64,
    },
    /// A visible overlay failed to reach its native window. Retry on a later
    /// event-loop turn without waiting for another terminal input event.
    GpuiOverlayRelayoutRetry {
        generation: u64,
        token: u64,
        attempt: u8,
    },
    /// Figterm intercepted a key bound to an autocomplete action.
    AutocompleteAction {
        action: String,
        session_id: uuid::Uuid,
    },
    /// A mouse click carries the exact row that was clicked. Keeping this in
    /// the event avoids a shared pending slot being overwritten by a second
    /// click before the host event loop handles the first one.
    AutocompleteClick {
        click: fastab_gpui::ClickInsert,
        session_id: uuid::Uuid,
        generation: u64,
    },

    ShowMessageNotification(ShowMessageNotification),
}

impl From<PlatformBoundEvent> for Event {
    fn from(event: PlatformBoundEvent) -> Self {
        Self::PlatformBoundEvent(event)
    }
}

impl From<ShowMessageNotification> for Event {
    fn from(event: ShowMessageNotification) -> Self {
        Self::ShowMessageNotification(event)
    }
}

#[derive(Debug, Default)]
pub struct ShowMessageNotification {
    pub title: Cow<'static, str>,
    pub body: Cow<'static, str>,
    pub parent: Option<WindowId>,
    pub buttons: Option<rfd::MessageButtons>,
    pub buttons_result: Option<tokio::sync::mpsc::Sender<rfd::MessageDialogResult>>,
}

#[derive(Debug, Clone)]
pub enum EmitEventName {
    Notification,
    ProtoMessageReceived,
    GlobalErrorOccurred,
}

impl std::fmt::Display for EmitEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Notification | Self::ProtoMessageReceived => "FigProtoMessageReceived",
            Self::GlobalErrorOccurred => "FigGlobalErrorOccurred",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowPosition {
    Absolute(Position),
    Centered,
    RelativeToCaret {
        caret_position: Position,
        caret_size: Size,
        origin: Origin,
    },
}

pub struct WindowGeometryResult {
    pub is_above: bool,
    pub is_clipped: bool,
}

#[derive(Debug, Clone)]
pub enum WindowEvent {
    /// Sets the window to be enabled or disabled
    ///
    /// This will cause events to be ignored other than [`WindowEvent::Hide`] and
    /// [`WindowEvent::SetEnabled(true)`]
    SetEnabled(bool),
    /// Sets the theme of the window (light, dark, or system if None)
    SetTheme(Option<Theme>),
    /// A local caret hook, kept distinct from native AX geometry until consumed.
    CaretPositionHook(WindowPosition),
    #[cfg(target_os = "macos")]
    TerminalEnabled {
        app: macos_utils::window_server::ApplicationSpecifier,
        cache_identity: Option<(i32, u32)>,
        epoch: u64,
        enabled: bool,
    },
    #[cfg(target_os = "macos")]
    TerminalCaret {
        app: macos_utils::window_server::ApplicationSpecifier,
        cache_identity: Option<(i32, u32)>,
        epoch: u64,
        position: Option<WindowPosition>,
        /// The owning window was destroyed; a later caret must not resume its input.
        invalidate_input: bool,
    },
    UpdateWindowGeometry {
        position: Option<WindowPosition>,
        size: Option<LogicalSize<f64>>,
        anchor: Option<LogicalSize<f64>>,
        dry_run: bool,
        tx: Option<UnboundedSender<WindowGeometryResult>>,
    },
    /// Hides the window.
    Hide,
    /// Closes the window.
    Close,
    Show,
    Emit {
        event_name: EmitEventName,
        payload: Cow<'static, str>,
    },
    NavigateRelative {
        path: Cow<'static, str>,
    },

    Event {
        event_name: Cow<'static, str>,
        payload: Option<Cow<'static, str>>,
    },

    Reload,

    Devtools,
    DebugMode(bool),

    Batch(Vec<WindowEvent>),
}

impl WindowEvent {
    pub fn is_allowed_while_disabled(&self) -> bool {
        matches!(
            self,
            WindowEvent::Hide
                | WindowEvent::Close
                | WindowEvent::SetEnabled(_)
                // TODO: we really shouldnt need to allow this to be called when disabled,
                // however we allow it at the moment because notification listeners are
                // initialized early on and we dont have a way to delay them until the window
                // is enabled
                | WindowEvent::Emit { .. }
        )
    }
}
