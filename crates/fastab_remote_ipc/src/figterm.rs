use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fastab_proto::fig::EnvironmentVariable;
use fastab_proto::local::{ShellContext, TerminalCursorCoordinates};
use fastab_proto::remote::hostbound;
use parking_lot::lock_api::MutexGuard;
use parking_lot::{FairMutex, MappedFairMutexGuard, RawFairMutex};
use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Clone, Default, Debug)]
pub struct EditBuffer {
    pub text: String,
    pub cursor: i64,
}

impl EditBuffer {
    pub fn update(&mut self, text: &str, cursor: i64) {
        // Keep normal keystrokes allocation-free, but do not pin an unusually
        // large input for the lifetime of a now-small/empty shell session.
        if self.text.capacity() > 64 * 1024 && text.len() < self.text.capacity() / 4 {
            self.text = text.to_owned();
        } else {
            self.text.clear();
            self.text.push_str(text);
        }
        self.cursor = cursor;
    }

    pub fn end_input(&mut self) {
        self.update("", 0);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetrics {
    pub start_time: OffsetDateTime,
    pub end_time: OffsetDateTime,
    pub num_insertions: i64,
    pub num_popups: i64,
}

impl SessionMetrics {
    pub fn new(start: OffsetDateTime) -> Self {
        Self {
            start_time: start,
            end_time: start,
            num_insertions: 0,
            num_popups: 0,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct InnerFigtermState {
    /// All current sessions of [FigtermSession]'s.
    pub linked_sessions: HashMap<Uuid, FigtermSession>,
    /// The most recent figterm session
    pub most_recent: Option<Uuid>,
}

#[derive(Debug, Default, Serialize)]
pub struct FigtermState {
    #[serde(flatten)]
    pub inner: FairMutex<InnerFigtermState>,
}

impl FigtermState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a new session id
    pub fn insert(&self, session: FigtermSession) {
        let mut figterm_state = self.inner.lock();
        figterm_state.most_recent = Some(session.id);
        figterm_state.linked_sessions.insert(session.id, session);
    }

    /// Gets mutable reference to the given session id and sets the most recent session id
    pub fn with_update<T>(&self, key: Uuid, f: impl FnOnce(&mut FigtermSession) -> T) -> Option<T> {
        let mut guard = self.inner.lock();
        let res = guard
            .linked_sessions
            .get_mut(&key)
            .and_then(|session| match session.dead_since {
                Some(_) => None,
                None => Some(f(session)),
            });

        if res.is_some() {
            guard.most_recent = Some(key);
        }

        res
    }

    pub fn with_most_recent<T>(&self, f: impl FnOnce(&mut FigtermSession) -> T) -> Option<T> {
        let mut guard = self.inner.lock();
        let id = guard.most_recent?;
        guard
            .linked_sessions
            .get_mut(&id)
            .and_then(|session| match session.dead_since {
                Some(_) => None,
                None => Some(f(session)),
            })
    }

    /// Gets mutable reference to the given session id
    pub fn with<T>(&self, session_id: &Uuid, f: impl FnOnce(&mut FigtermSession) -> T) -> Option<T> {
        let mut guard = self.inner.lock();
        guard.linked_sessions.get_mut(session_id).map(f)
    }

    pub fn get(&self, session_id: &Uuid) -> Option<MappedFairMutexGuard<'_, FigtermSession>> {
        MutexGuard::<'_, RawFairMutex, InnerFigtermState>::try_map(
            self.inner.lock(),
            |guard: &mut InnerFigtermState| guard.linked_sessions.get_mut(session_id),
        )
        .ok()
    }

    pub fn most_recent(&self) -> Option<MappedFairMutexGuard<'_, FigtermSession>> {
        MutexGuard::<'_, RawFairMutex, InnerFigtermState>::try_map(
            self.inner.lock(),
            |guard: &mut InnerFigtermState| {
                guard
                    .most_recent
                    .as_mut()
                    .and_then(|id| guard.linked_sessions.get_mut(id))
            },
        )
        .ok()
    }

    pub fn with_maybe_id<T>(&self, session_id: &Option<Uuid>, f: impl FnOnce(&mut FigtermSession) -> T) -> Option<T> {
        match session_id {
            Some(session_id) => self.with(session_id, f),
            None => self.with_most_recent(f),
        }
    }

    pub fn remove_id(&self, session_id: &Uuid) -> Option<FigtermSession> {
        let mut guard = self.inner.lock();
        if guard.most_recent.as_ref() == Some(session_id) {
            guard.most_recent = None;
        }
        guard.linked_sessions.remove(session_id)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum InterceptMode {
    Locked,
    Unlocked,
}

impl From<bool> for InterceptMode {
    fn from(from: bool) -> Self {
        if from {
            InterceptMode::Locked
        } else {
            InterceptMode::Unlocked
        }
    }
}

impl From<InterceptMode> for bool {
    fn from(from: InterceptMode) -> Self {
        match from {
            InterceptMode::Locked => true,
            InterceptMode::Unlocked => false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FigtermSession {
    pub id: Uuid,
    pub secret: String,
    #[serde(skip)]
    pub sender: crate::outbox::FigtermSender,
    #[serde(skip)]
    pub dead_since: Option<Instant>, // TODO: prune old sessions
    #[serde(skip)]
    pub edit_buffer: EditBuffer,
    #[serde(skip)]
    pub last_receive: Instant,
    pub context: Option<ShellContext>,
    /// Flattened once when context env actually changes. Edit-buffer hooks
    /// omit env, so this Arc is cloned into completion requests instead of
    /// rebuilding the pair list on every keystroke.
    #[serde(skip)]
    pub flattened_env: Arc<Vec<(String, String)>>,
    #[serde(skip)]
    pub terminal_cursor_coordinates: Option<TerminalCursorCoordinates>,
    pub current_session_metrics: Option<SessionMetrics>,
    pub intercept: InterceptMode,
    pub intercept_global: InterceptMode,
}

impl FigtermSession {
    pub fn send(&mut self, command: FigtermCommand) -> Result<(), crate::outbox::SendError> {
        let insertion = matches!(&command, FigtermCommand::InsertText { .. });
        self.sender.send(command)?;
        if insertion {
            if let Some(metrics) = &mut self.current_session_metrics {
                metrics.num_insertions += 1;
                metrics.end_time = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct FigtermSessionInfo {
    pub edit_buffer: EditBuffer,
    pub context: Option<ShellContext>,
}

impl FigtermSession {
    #[allow(dead_code)]
    pub fn get_info(&self) -> FigtermSessionInfo {
        FigtermSessionInfo {
            edit_buffer: self.edit_buffer.clone(),
            context: self.context.clone(),
        }
    }

    /// Merge session context. Edit-buffer frames send cwd / process / shell
    /// path, and env/alias only after `UpdateShellContext`; a `None`
    /// incoming hook is a no-op so a missing frame cannot wipe a prompt.
    pub fn apply_context(&mut self, incoming: Option<ShellContext>) {
        let Some(new) = incoming else {
            return;
        };
        match self.context.as_mut() {
            Some(existing) => {
                if let Some(pid) = new.pid {
                    existing.pid = Some(pid);
                }
                merge_if_some(&mut existing.ttys, new.ttys);
                merge_if_some(&mut existing.process_name, new.process_name);
                merge_if_some(&mut existing.current_working_directory, new.current_working_directory);
                merge_if_some(&mut existing.session_id, new.session_id);
                merge_if_some(&mut existing.terminal, new.terminal);
                merge_if_some(&mut existing.hostname, new.hostname);
                merge_if_some(&mut existing.shell_path, new.shell_path);
                merge_if_some(&mut existing.wsl_distro, new.wsl_distro);
                merge_if_some(&mut existing.qterm_version, new.qterm_version);
                if let Some(preexec) = new.preexec {
                    existing.preexec = Some(preexec);
                }
                if let Some(osc_lock) = new.osc_lock {
                    existing.osc_lock = Some(osc_lock);
                }
                merge_if_some(&mut existing.alias, new.alias);
                if !new.environment_variables.is_empty() {
                    existing.environment_variables = new.environment_variables;
                    self.flattened_env = flatten_shell_environment(self.context.as_ref());
                }
            },
            None => {
                self.context = Some(new);
                self.flattened_env = flatten_shell_environment(self.context.as_ref());
            },
        }
    }
}

fn merge_if_some<T>(existing: &mut Option<T>, incoming: Option<T>) {
    if incoming.is_some() {
        *existing = incoming;
    }
}

fn flatten_shell_environment(context: Option<&ShellContext>) -> Arc<Vec<(String, String)>> {
    let Some(context) = context else {
        return Arc::new(Vec::new());
    };
    Arc::new(
        context
            .environment_variables
            .iter()
            .filter_map(|variable| {
                let value = variable.value.clone()?;
                (!variable.key.is_empty()).then(|| (variable.key.clone(), value))
            })
            .collect(),
    )
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum FigtermCommand {
    InterceptFigJs {
        intercept_keystrokes: bool,
        intercept_global_keystrokes: bool,
        actions: Vec<fastab_proto::figterm::Action>,
        override_actions: bool,
    },
    InterceptFigJSVisible {
        visible: bool,
    },
    InsertText {
        insertion: Option<String>,
        deletion: Option<i64>,
        offset: Option<i64>,
        immediate: Option<bool>,
        insertion_buffer: Option<String>,
        insert_during_command: Option<bool>,
    },
    SetBuffer {
        text: String,
        cursor_position: Option<u64>,
    },
    RunProcess {
        channel: oneshot::Sender<hostbound::response::Response>,
        executable: String,
        arguments: Vec<String>,
        working_directory: Option<String>,
        env: Vec<EnvironmentVariable>,
        timeout: Option<Duration>,
    },
}

macro_rules! field {
    ($fn_name:ident: $enum_name:ident, $($field_name: ident: $field_type: ty),*,) => {
        pub fn $fn_name($($field_name: $field_type),*) -> (Self, oneshot::Receiver<hostbound::response::Response>) {
            let (tx, rx) = oneshot::channel();
            (Self::$enum_name {channel: tx, $($field_name),*}, rx)
        }
    };
}

impl FigtermCommand {
    field!(
        run_process: RunProcess,
        executable: String,
        arguments: Vec<String>,
        working_directory: Option<String>,
        env: Vec<EnvironmentVariable>,
        timeout: Option<Duration>,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastab_proto::local::EnvironmentVariable;

    #[test]
    fn edit_buffer_keeps_normal_capacity_but_retires_large_inputs() {
        let mut buffer = EditBuffer::default();
        buffer.update("git checkout", 12);
        let small_capacity = buffer.text.capacity();
        buffer.update("git", 3);
        assert_eq!(buffer.text.capacity(), small_capacity);

        buffer.update(&"x".repeat(256 * 1024), 5);
        buffer.update("git 日", 7);
        assert_eq!(buffer.text, "git 日");
        assert_eq!(buffer.cursor, 7);
        assert!(buffer.text.capacity() < 64 * 1024);

        buffer.update(&"x".repeat(256 * 1024), 5);
        buffer.end_input();
        assert!(buffer.text.is_empty());
        assert_eq!(buffer.text.capacity(), 0);
        assert_eq!(buffer.cursor, 0);
    }

    fn dummy_session() -> FigtermSession {
        let (sender, _outbox) = crate::outbox::channel();
        FigtermSession {
            id: Uuid::nil(),
            secret: String::new(),
            sender,
            dead_since: None,
            edit_buffer: EditBuffer::default(),
            last_receive: Instant::now(),
            context: None,
            flattened_env: Arc::new(Vec::new()),
            terminal_cursor_coordinates: None,
            current_session_metrics: None,
            intercept: InterceptMode::Unlocked,
            intercept_global: InterceptMode::Unlocked,
        }
    }

    fn context_with_env(key: &str, value: &str) -> ShellContext {
        ShellContext {
            environment_variables: vec![EnvironmentVariable {
                key: key.into(),
                value: Some(value.into()),
            }],
            alias: Some("ll='ls -l'".into()),
            ..Default::default()
        }
    }

    #[test]
    fn apply_context_keeps_env_when_the_edit_buffer_omits_it() {
        let mut session = dummy_session();
        session.apply_context(Some(context_with_env("PATH", "/bin")));
        assert_eq!(session.flattened_env.as_slice(), &[("PATH".into(), "/bin".into())]);

        session.apply_context(Some(ShellContext {
            process_name: Some("zsh".into()),
            ..Default::default()
        }));
        assert_eq!(session.context.as_ref().unwrap().process_name.as_deref(), Some("zsh"));
        assert_eq!(session.context.as_ref().unwrap().alias.as_deref(), Some("ll='ls -l'"));
        assert_eq!(session.flattened_env.as_slice(), &[("PATH".into(), "/bin".into())]);
    }

    #[test]
    fn apply_context_replaces_env_when_the_prompt_sends_a_new_one() {
        let mut session = dummy_session();
        session.apply_context(Some(context_with_env("PATH", "/bin")));
        session.apply_context(Some(context_with_env("PATH", "/usr/bin")));
        assert_eq!(session.flattened_env.as_slice(), &[("PATH".into(), "/usr/bin".into())]);
    }

    #[test]
    fn apply_context_reuses_the_flattened_env_arc_when_env_is_omitted() {
        let mut session = dummy_session();
        session.apply_context(Some(context_with_env("PATH", "/bin")));
        let first = Arc::as_ptr(&session.flattened_env);
        session.apply_context(Some(ShellContext {
            current_working_directory: Some("/tmp".into()),
            ..Default::default()
        }));
        assert_eq!(
            session
                .context
                .as_ref()
                .and_then(|ctx| ctx.current_working_directory.as_deref()),
            Some("/tmp")
        );
        assert_eq!(session.context.as_ref().unwrap().alias.as_deref(), Some("ll='ls -l'"));
        assert!(std::ptr::eq(first, Arc::as_ptr(&session.flattened_env)));
    }

    #[test]
    fn apply_context_none_does_not_wipe_the_session() {
        let mut session = dummy_session();
        session.apply_context(Some(context_with_env("PATH", "/bin")));
        session.apply_context(None);
        assert_eq!(session.flattened_env.as_slice(), &[("PATH".into(), "/bin".into())]);
        assert!(session.context.is_some());
    }
}
