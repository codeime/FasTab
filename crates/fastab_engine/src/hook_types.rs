//! Shared hook types used by the native backend (typed IR + named adapters).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

/// The subset of `Fig.ShellContext` that generators actually read.
#[derive(Debug, Clone, Default)]
pub struct ShellContext {
    pub current_process: String,
    pub environment_variables: HookEnvironment,
}

/// One request's immutable environment. Individual lookups borrow the last
/// value directly; only enumeration builds a sorted, deduplicated index. Hook
/// contexts share both the original strings and that index, never copy them.
#[derive(Debug, Clone, Default)]
pub struct HookEnvironment(Arc<EnvironmentData>);

#[derive(Debug, Default)]
struct EnvironmentData {
    source: Arc<Vec<(String, String)>>,
    ordered: OnceLock<Vec<usize>>,
}

impl From<Arc<Vec<(String, String)>>> for HookEnvironment {
    fn from(source: Arc<Vec<(String, String)>>) -> Self {
        Self(Arc::new(EnvironmentData {
            source,
            ordered: OnceLock::new(),
        }))
    }
}

impl HookEnvironment {
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.0.source
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.0
            .source
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        let source = &self.0.source;
        let indexes = self.0.ordered.get_or_init(|| {
            let mut indexes: Vec<_> = (0..source.len()).collect();
            // For duplicate names keep the last original value, matching the
            // BTreeMap previously built by HookContext::from_shell.
            indexes.sort_unstable_by(|a, b| source[*a].0.cmp(&source[*b].0).then_with(|| b.cmp(a)));
            indexes.dedup_by(|a, b| source[*a].0 == source[*b].0);
            indexes
        });
        indexes.iter().map(|index| {
            let (key, value) = &source[*index];
            (key, value)
        })
    }
}

impl PartialEq for HookEnvironment {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.iter().eq(other.iter())
    }
}

impl Eq for HookEnvironment {}

impl Serialize for HookEnvironment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for HookEnvironment {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = BTreeMap::<String, String>::deserialize(deserializer)?;
        Ok(Arc::new(values.into_iter().collect::<Vec<_>>()).into())
    }
}

#[derive(Debug, Clone)]
pub struct ScriptCommand {
    pub command: String,
    pub args: Vec<String>,
    pub timeout_ms: Option<i64>,
}

/// Development/test diagnostic side channel. The normal hook APIs keep their
/// historical `Option` returns so a missing source, a failed evaluation, and
/// a valid empty result do not look identical. It intentionally carries no
/// hook input, cwd, shell, environment, command, or error text.
///
/// Every variant is reachable. The QuickJS-era promise and runtime outcomes
/// were dropped with the runtime; do not re-add a variant nothing records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDiagnostic {
    Success,
    EmptyResult,
    SourceMissing,
    InvokeError,
    Timeout,
    ResultConversionError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDiagnosticRecord {
    pub hook_id: String,
    pub outcome: HookDiagnostic,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HookContext {
    #[serde(rename = "currentWorkingDirectory")]
    pub current_working_directory: String,
    #[serde(rename = "currentProcess")]
    pub current_process: String,
    #[serde(rename = "sshPrefix")]
    pub ssh_prefix: String,
    #[serde(rename = "environmentVariables")]
    pub environment_variables: HookEnvironment,
    #[serde(rename = "searchTerm")]
    pub search_term: String,
    #[serde(rename = "isDangerous")]
    pub is_dangerous: bool,
}

impl HookContext {
    pub fn from_shell(cwd: &str, shell: &ShellContext, search_term: &str, is_dangerous: bool) -> Self {
        Self {
            current_working_directory: cwd.to_string(),
            current_process: shell.current_process.clone(),
            ssh_prefix: String::new(),
            environment_variables: shell.environment_variables.clone(),
            search_term: search_term.to_string(),
            is_dangerous,
        }
    }
}

pub fn clean_output(output: &str) -> String {
    output
        .replace("\r\n", "\n")
        .replace("\x1b[?25h", "")
        .trim_start_matches('\n')
        .trim_end_matches('\n')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_contexts_share_environment_strings_and_materialize_only_ordered_indices() {
        let source = Arc::new(vec![
            ("Z".into(), "last key".into()),
            ("A".into(), "obsolete".into()),
            ("A".into(), "final value".into()),
        ]);
        let shell = ShellContext {
            current_process: "zsh".into(),
            environment_variables: source.clone().into(),
        };
        let first = HookContext::from_shell("/repo", &shell, "", false);
        let second = HookContext::from_shell("/repo", &shell, "a", false);
        let env = &first.environment_variables;
        assert!(Arc::ptr_eq(&env.0, &second.environment_variables.0));
        assert!(Arc::ptr_eq(&env.0.source, &source));
        assert_eq!(env.get("A").map(String::as_str), Some("final value"));
        assert!(env.get("absent").is_none());
        assert!(env.0.ordered.get().is_none(), "single-key reads need no full index");
        assert_eq!(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
            [("A", "final value"), ("Z", "last key")]
        );
        let value = env.iter().next().unwrap().1;
        assert_eq!(
            value.as_ptr(),
            source[2].1.as_ptr(),
            "enumeration borrows the source value"
        );
        assert_eq!(second.environment_variables.0.ordered.get(), Some(&vec![2, 0]));
        let json = serde_json::to_value(&first).unwrap();
        assert_eq!(
            json["environmentVariables"],
            serde_json::json!({"A": "final value", "Z": "last key"})
        );
        assert_eq!(serde_json::from_value::<HookContext>(json).unwrap(), first);
    }
}
