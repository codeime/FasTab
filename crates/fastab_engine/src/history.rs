//! Spec-aware `template: "history"` values — Fig's `getHistoryArgSuggestions`.
//!
//! The WebView parsed every history line against its spec once, indexed each
//! argument value by the slot it filled (subcommand path plus positional
//! index, or option plus option-argument index), and answered a `history`
//! template by walking the *current* buffer to the same kind of slot. So
//! `curl -X POST <url>` contributes `<url>` to `curl`'s URL argument even
//! though the buffer being typed is `curl <TAB>`; a plain "same leading
//! tokens" match would have offered `-X` instead, which is what the first
//! Rust port did.
//!
//! The index is keyed by the spec that owns the slot, not by the first token
//! of the line: Fig split each parsed line into one annotation run per
//! `loadSpec` / `isCommand` switch, so `sudo curl <url>` filed `<url>` under
//! `curl`. Lines are parsed lazily per spec and cached until the history
//! itself or its alias/shell parsing context changes. Fig parsed everything up front in the background,
//! which took seconds; here the first `ssh <TAB>` pays only for the lines
//! that mention `ssh`.
//!
//! Slot indices follow `walkSubcommand` exactly: the n-th positional token
//! in a scope looks up `args[n]` and the n-th value after an option looks up
//! `option.args[n]`, with no clamping for variadic arguments. The second
//! value of a variadic argument therefore has no slot and is neither indexed
//! nor offered, which is what the WebView did.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::ir::{OptionSpec, Registry, Spec};

/// Where an argument value sits in a spec: the spec that owns it, the
/// subcommand path below that spec, and either a positional index or an
/// option with its argument index. Names are primary names so `co` and
/// `checkout` share a slot — Fig's mirrored index maps every alias of a
/// subcommand or option to one node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ArgSlot {
    pub root: String,
    pub path: Vec<String>,
    pub option: Option<String>,
    pub index: usize,
}

fn primary_name(names: &[String]) -> String {
    names.iter().find(|name| !name.is_empty()).cloned().unwrap_or_default()
}

/// Recorder the spec walker fills in as it consumes tokens.
#[derive(Debug, Default)]
pub(crate) struct WalkTrace {
    root: String,
    path: Vec<String>,
    /// `(slot, value)` for every argument token the walker consumed, in
    /// buffer order.
    pub values: Vec<(ArgSlot, String)>,
    /// Slots the token being typed would fill: the positional one first,
    /// then the option argument. Fig ran `walkSubcommand` once per type and
    /// concatenated in that order.
    pub current: Vec<ArgSlot>,
}

impl WalkTrace {
    pub fn new(root: &Spec) -> Self {
        Self {
            root: primary_name(&root.names),
            ..Self::default()
        }
    }

    /// A `loadSpec` / `isCommand` switch: values after this belong to `spec`.
    pub fn enter_root(&mut self, spec: &Spec) {
        self.root = primary_name(&spec.names);
        self.path.clear();
    }

    pub fn enter_subcommand(&mut self, spec: &Spec) {
        self.path.push(primary_name(&spec.names));
    }

    pub(crate) fn slot(&self, option: Option<&OptionSpec>, index: usize) -> ArgSlot {
        ArgSlot {
            root: self.root.clone(),
            path: self.path.clone(),
            option: option.map(|option| primary_name(&option.names)),
            index,
        }
    }

    /// `value` was the `index`-th positional token of the current scope.
    /// Nothing is recorded past the spec's argument list, matching
    /// `args[subcommandArgIndex]` coming back `undefined`.
    pub fn record_positional(&mut self, args: &[crate::ir::ArgSpec], index: usize, value: &str) {
        if index < args.len() {
            let slot = self.slot(None, index);
            self.values.push((slot, value.to_string()));
        }
    }

    /// `value` was the `index`-th value after `option`.
    pub fn record_option_arg(&mut self, option: &OptionSpec, index: usize, value: &str) {
        if index < option.args.len() {
            let slot = self.slot(Some(option), index);
            self.values.push((slot, value.to_string()));
        }
    }

    pub fn set_current(
        &mut self,
        args: &[crate::ir::ArgSpec],
        positional: usize,
        option_arg: Option<(&OptionSpec, usize)>,
    ) {
        self.current.clear();
        if positional < args.len() {
            self.current.push(self.slot(None, positional));
        }
        if let Some((option, index)) = option_arg
            && index < option.args.len()
        {
            self.current.push(self.slot(Some(option), index));
        }
    }
}

type SlotIndex = HashMap<ArgSlot, Vec<String>>;

/// How many spec argument indexes stay built. A shell session only completes
/// a handful of commands; an index that falls out of this window is rebuilt
/// from the same lines the next time that command is completed.
const MAX_CACHED_INDEXES: usize = 32;
// Same retained payload budget as one hook cache; oversized indexes remain
// available to the current request but are rebuilt instead of cached.
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct CachedIndex {
    value: Arc<SlotIndex>,
    bytes: usize,
}

fn slot_index_bytes(index: &SlotIndex) -> usize {
    index.capacity() * std::mem::size_of::<(ArgSlot, Vec<String>)>()
        + index
            .iter()
            .map(|(slot, values)| {
                slot.root.capacity()
                    + slot.path.capacity() * std::mem::size_of::<String>()
                    + slot.path.iter().map(String::capacity).sum::<usize>()
                    + slot.option.as_ref().map_or(0, String::capacity)
                    + values.capacity() * std::mem::size_of::<String>()
                    + values.iter().map(String::capacity).sum::<usize>()
            })
            .sum::<usize>()
}

#[derive(Debug, Default)]
struct IndexCache {
    entries: HashMap<String, CachedIndex>,
    recent: VecDeque<String>,
    aliases: Option<String>,
    shell: Option<String>,
    allocated_bytes: usize,
}

impl IndexCache {
    fn matches_context(&self, aliases: Option<&str>, shell: Option<&str>) -> bool {
        self.aliases.as_deref() == aliases && self.shell.as_deref() == shell
    }

    fn select_context(&mut self, aliases: Option<&str>, shell: Option<&str>) {
        if !self.matches_context(aliases, shell) {
            *self = Self::default();
            if aliases.map_or(0, str::len).saturating_add(shell.map_or(0, str::len)) > MAX_INDEX_BYTES {
                return;
            }
            self.aliases = aliases.map(str::to_owned);
            self.shell = shell.map(str::to_owned);
            if self.context_bytes() > MAX_INDEX_BYTES {
                // Do not retain an oversized parsing context. publish_index
                // still returns this request's complete result without caching.
                *self = Self::default();
            }
        }
    }

    fn context_bytes(&self) -> usize {
        self.aliases.as_ref().map_or(0, String::capacity) + self.shell.as_ref().map_or(0, String::capacity)
    }

    fn has_derived_payload(&self) -> bool {
        !self.entries.is_empty() || self.aliases.is_some() || self.shell.is_some()
    }

    fn remove(&mut self, name: &str) {
        if let Some(entry) = self.entries.remove(name) {
            self.allocated_bytes -= entry.bytes;
        }
    }

    fn get(&mut self, name: &str) -> Option<Arc<SlotIndex>> {
        let index = Arc::clone(&self.entries.get(name)?.value);
        self.touch(name);
        Some(index)
    }

    fn insert(&mut self, name: String, index: Arc<SlotIndex>) {
        self.remove(&name);
        self.recent.retain(|existing| existing != &name);
        let bytes = 2 * (std::mem::size_of::<String>() + name.capacity())
            + std::mem::size_of::<CachedIndex>()
            + std::mem::size_of::<SlotIndex>()
            + 2 * std::mem::size_of::<usize>()
            + slot_index_bytes(&index);
        if bytes > MAX_INDEX_BYTES.saturating_sub(self.context_bytes()) {
            return;
        }
        while self.entries.len() >= MAX_CACHED_INDEXES
            || self.allocated_bytes + bytes > MAX_INDEX_BYTES - self.context_bytes()
        {
            let Some(old) = self.recent.pop_front() else {
                break;
            };
            self.remove(&old);
        }
        self.recent.push_back(name.clone());
        self.allocated_bytes += bytes;
        self.entries.insert(name, CachedIndex { value: index, bytes });
    }

    fn touch(&mut self, name: &str) {
        if let Some(position) = self.recent.iter().position(|item| item == name) {
            let existing = self.recent.remove(position).expect("position came from recent");
            self.recent.push_back(existing);
            return;
        }
        self.recent.push_back(name.to_string());
    }
}

/// Shell history as the engine loaded it, plus the per-spec argument index
/// built from it on demand. One instance lives on the engine for as long as
/// that history load is current; requests borrow it through an `Arc`, so a
/// keystroke never copies the line list.
#[derive(Debug, Default)]
pub(crate) struct HistoryStore {
    lines: Arc<Vec<Arc<str>>>,
    /// Spec primary name → values by slot, capped at [`MAX_CACHED_INDEXES`].
    index: Mutex<IndexCache>,
    #[cfg(test)]
    arg_values_calls: std::sync::atomic::AtomicUsize,
}

impl HistoryStore {
    pub fn new(lines: Arc<Vec<Arc<str>>>) -> Self {
        Self {
            lines,
            index: Mutex::new(IndexCache::default()),
            #[cfg(test)]
            arg_values_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    pub fn from_lines(lines: Vec<String>) -> Self {
        Self::new(Arc::new(lines.into_iter().map(Arc::<str>::from).collect()))
    }

    /// The history lines this store indexes; the engine compares this by
    /// pointer to decide whether a reload has replaced them.
    pub fn lines(&self) -> &Arc<Vec<Arc<str>>> {
        &self.lines
    }

    pub(crate) fn has_derived_indexes(&self) -> bool {
        self.index
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .has_derived_payload()
    }

    pub(crate) fn release_derived_indexes(&self) {
        *self.index.lock().unwrap_or_else(|error| error.into_inner()) = IndexCache::default();
    }

    pub(crate) fn diagnostics(&self) -> crate::diagnostics::HistoryDiagnostics {
        let cache = self.index.lock().unwrap_or_else(|error| error.into_inner());
        let mut seen = HashSet::new();
        let line_bytes = self.lines.capacity() * std::mem::size_of::<Arc<str>>()
            + self
                .lines
                .iter()
                .filter(|line| seen.insert(Arc::as_ptr(line)))
                .map(|line| line.len() + 2 * std::mem::size_of::<usize>())
                .sum::<usize>();
        let index_bytes = cache.allocated_bytes + cache.context_bytes();
        crate::diagnostics::HistoryDiagnostics {
            line_count: self.lines.len(),
            index_count: cache.entries.len(),
            allocated_bytes: line_bytes + index_bytes,
            index_bytes,
            ..Default::default()
        }
    }

    /// Values previously typed into any of `slots`, most recent first,
    /// deduplicated across slots. `aliases` and `shell` are the request's, so
    /// history lines expand the same way the buffer does.
    pub fn arg_values(
        &self,
        registry: &mut Registry,
        aliases: Option<&str>,
        shell: Option<&str>,
        slots: &[ArgSlot],
    ) -> Vec<String> {
        #[cfg(test)]
        self.arg_values_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut seen = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for slot in slots {
            if slot.root.is_empty() {
                continue;
            }
            let index = self.index_for(registry, &slot.root, aliases, shell);
            let Some(values) = index.get(slot) else {
                continue;
            };
            for value in values.iter().rev() {
                if seen.insert(value.clone()) {
                    out.push(value.clone());
                }
            }
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn arg_values_calls(&self) -> usize {
        self.arg_values_calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn index_for(
        &self,
        registry: &mut Registry,
        root_name: &str,
        aliases: Option<&str>,
        shell: Option<&str>,
    ) -> Arc<SlotIndex> {
        match crate::cancellation::commit_if_active(|| {
            let mut cache = self.index.lock().unwrap_or_else(|err| err.into_inner());
            cache.select_context(aliases, shell);
            cache.get(root_name)
        }) {
            Ok(Some(index)) => return index,
            Ok(None) => {},
            Err(_) => return Arc::default(),
        }
        let built = Arc::new(build_index(&self.lines, registry, root_name, aliases, shell));
        self.publish_index(root_name, aliases, shell, built)
    }

    fn publish_index(
        &self,
        root_name: &str,
        aliases: Option<&str>,
        shell: Option<&str>,
        built: Arc<SlotIndex>,
    ) -> Arc<SlotIndex> {
        if crate::cancellation::is_cancelled() {
            return Arc::default();
        }
        crate::cancellation::commit_if_active(|| {
            let mut cache = self.index.lock().unwrap_or_else(|err| err.into_inner());
            // Another request may select a different context while this index is
            // built outside the lock. It may use its own result, but cannot publish
            // it into that request's cache or return an index from that context.
            if !cache.matches_context(aliases, shell) {
                return built;
            }
            if let Some(index) = cache.get(root_name) {
                return index;
            }
            cache.insert(root_name.to_string(), Arc::clone(&built));
            built
        })
        .unwrap_or_default()
    }
}

/// Shell operators that end one command and start another. Fig ran the
/// full shell parser (`getAllCommandsWithAlias`) over each line; splitting
/// on the operator tokens the tokenizer already isolates covers the same
/// `a && b`, `a | b`, `a; b` shapes.
fn is_command_separator(token: &str) -> bool {
    matches!(token, "&&" | "||" | "|" | ";" | "&")
}

/// Whether `token` could reach the `root_name` spec: the bare name or a path
/// ending in it, which is how `root_spec_for_command` resolves `/usr/bin/curl`.
fn names_spec(token: &str, root_name: &str) -> bool {
    token == root_name || crate::lookup::local_spec_name(token) == root_name
}

fn build_index(
    lines: &[Arc<str>],
    registry: &mut Registry,
    root_name: &str,
    aliases: Option<&str>,
    shell: Option<&str>,
) -> SlotIndex {
    // The whole build is a side load, including the line's first token.
    // `sudo git …` leaves `sudo` on the grace clock; only the edit buffer keeps a file in use.
    let _pause = registry.pause_idle_touch();
    let mut index: SlotIndex = HashMap::new();
    let aliases = aliases.map(|raw| crate::lookup::parse_alias_map(raw, shell));
    for line in lines {
        if crate::cancellation::is_cancelled() {
            return SlotIndex::new();
        }
        let (tokens, _) = crate::lookup::tokenize(line);
        for command in tokens.split(|token| is_command_separator(token)) {
            if command.is_empty() {
                continue;
            }
            let mut command = command.to_vec();
            if let Some(aliases) = aliases.as_ref() {
                crate::lookup::expand_alias_tokens_with(&mut command, true, aliases);
            }
            // Only lines that mention the spec can file values under it; a
            // wrapper such as `sudo curl …` reaches `curl` through its own
            // spec, so the walk starts from the line's first token.
            if !command.iter().any(|token| names_spec(token, root_name)) {
                continue;
            }
            let Some(root) = registry.get_arc(crate::lookup::local_spec_name(&command[0])) else {
                continue;
            };
            let mut trace = WalkTrace::new(root.as_ref());
            // Fig parsed history with `exec` replaced by a function that
            // throws, so no hook could run a process; skipping hooks
            // outright is the same outcome without the failed attempts.
            crate::hook_backend::without_hooks(|| {
                crate::lookup::annotate_history_command(root, &mut command, registry, &mut trace);
            });
            for (slot, value) in trace.values {
                if slot.root != root_name || value.is_empty() {
                    continue;
                }
                index.entry(slot).or_default().push(value);
            }
        }
    }
    for values in index.values_mut() {
        // Queries visit each slot newest-first. Keep its last occurrence of
        // each value without changing the surviving values' original order.
        // The borrowed set dies before retain can move or drop any String.
        let keep = {
            let mut seen = HashSet::new();
            values
                .iter()
                .rev()
                .map(|value| seen.insert(value.as_str()))
                .collect::<Vec<_>>()
        };
        let mut keep = keep.into_iter().rev();
        values.retain(|_| keep.next().expect("one keep flag per history value"));
        values.shrink_to_fit();
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn registry_with(specs: &[(&str, &str)]) -> Registry {
        let dir = tempfile::tempdir().unwrap();
        for (name, spec) in specs {
            fs::write(dir.path().join(format!("{name}.json")), spec).unwrap();
        }
        let registry = Registry::load(dir.path()).unwrap();
        // Keep the temp dir alive for the registry's lazy loads.
        let _ = dir.keep();
        registry
    }

    const CURL: &str = r#"{
      "names": ["curl"],
      "options": [
        {"names": ["-X"], "args": [{"name": "method"}]},
        {"names": ["-v"]}
      ],
      "args": [{"name": "URL", "templates": ["history"]}]
    }"#;

    #[test]
    fn values_come_from_the_matching_slot_regardless_of_options_in_between() {
        let mut registry = registry_with(&[("curl", CURL)]);
        let store = HistoryStore::from_lines(vec![
            "curl https://old.example".into(),
            "curl -X POST https://api.example".into(),
            "curl -v -X GET https://other.example".into(),
            "wget https://not-curl.example".into(),
            "cd /tmp && curl https://after-and.example".into(),
        ]);
        let slot = ArgSlot {
            root: "curl".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        let values = store.arg_values(&mut registry, None, None, &[slot.clone()]);
        assert_eq!(
            values,
            vec![
                "https://after-and.example",
                "https://other.example",
                "https://api.example",
                "https://old.example",
            ]
        );

        let method = ArgSlot {
            option: Some("-X".into()),
            ..slot
        };
        let methods = store.arg_values(&mut registry, None, None, &[method]);
        assert_eq!(methods, vec!["GET", "POST"]);
    }

    #[test]
    fn argument_values_keep_slot_precedence_while_deduplicating_across_slots() {
        let mut registry = registry_with(&[("curl", CURL)]);
        let store = HistoryStore::from_lines(vec![
            "curl -X GET GET".into(),
            "curl -X POST POST".into(),
            "curl -X GET url".into(),
        ]);
        let url = ArgSlot {
            root: "curl".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        let method = ArgSlot {
            option: Some("-X".into()),
            ..url.clone()
        };
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[url.clone(), method.clone(), url.clone()]),
            ["url", "POST", "GET"]
        );
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[method, url]),
            ["GET", "POST", "url"]
        );
    }

    #[test]
    fn history_aliases_are_expanded_before_indexing() {
        let mut registry = registry_with(&[(
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [{"names": ["checkout", "co"], "args": [{"name": "branch"}]}]
            }"#,
        )]);
        let store = HistoryStore::from_lines(vec!["g co feature".into(), "git checkout main".into()]);
        let slot = ArgSlot {
            root: "git".into(),
            path: vec!["checkout".into()],
            option: None,
            index: 0,
        };
        let values = store.arg_values(&mut registry, Some("alias g='git'\n"), None, &[slot]);
        assert_eq!(values, vec!["main", "feature"]);
    }

    #[test]
    fn cancelled_history_build_cannot_publish_or_change_the_active_context() {
        let mut registry = registry_with(&[("curl", CURL)]);
        let store = HistoryStore::from_lines(vec!["curl https://example.test".into()]);
        let original = store.index_for(&mut registry, "curl", None, None);
        let built = Arc::new(build_index(&store.lines, &mut registry, "curl", None, None));
        let token = crate::cancellation::CancellationToken::new();
        let scope = crate::cancellation::enter(token.clone());
        token.cancel();
        assert!(store.publish_index("curl", None, None, built).is_empty());
        assert!(
            store
                .index_for(&mut registry, "curl", Some("alias c=curl"), Some("zsh"))
                .is_empty()
        );
        assert!(build_index(&store.lines, &mut registry, "curl", None, None).is_empty());
        drop(scope);
        assert!(Arc::ptr_eq(
            &original,
            &store.index_for(&mut registry, "curl", None, None)
        ));
    }

    #[test]
    fn changing_aliases_or_shell_rebuilds_the_same_history() {
        let mut registry = registry_with(&[("curl", CURL)]);
        let lines = vec!["c https://alias.example".into(), "curl https://plain.example".into()];
        let store = HistoryStore::from_lines(lines.clone());
        let contexts = [
            (Some("alias c='curl'"), Some("/bin/zsh")),
            (Some("alias c='wget'"), Some("/bin/zsh")),
            (Some("alias c 'curl'"), Some("/bin/zsh")),
            (Some("alias c 'curl'"), Some("/usr/local/bin/fish")),
            (None, None),
        ];
        let mut previous = None;
        for (aliases, shell) in contexts {
            let warm = store.index_for(&mut registry, "curl", aliases, shell);
            let cold = HistoryStore::from_lines(lines.clone()).index_for(&mut registry, "curl", aliases, shell);
            assert_eq!(*warm, *cold);
            let expected = if (aliases, shell) == contexts[0] || (aliases, shell) == contexts[3] {
                2
            } else {
                1
            };
            assert_eq!(warm.values().map(Vec::len).sum::<usize>(), expected);
            let again = store.index_for(&mut registry, "curl", aliases, shell);
            assert!(Arc::ptr_eq(&warm, &again));
            if let Some(previous) = previous {
                assert!(!Arc::ptr_eq(&warm, &previous));
            }
            previous = Some(warm);
        }
    }

    #[test]
    fn late_index_build_cannot_publish_into_a_different_context() {
        let mut registry = registry_with(&[("curl", CURL)]);
        let store = HistoryStore::from_lines(vec!["c https://alias.example".into()]);
        let aliases_a = Some("alias c='curl'");
        let aliases_b = Some("alias c='wget'");
        store.index.lock().unwrap().select_context(aliases_a, None);
        let built_a = Arc::new(build_index(&store.lines, &mut registry, "curl", aliases_a, None));
        let current_b = store.index_for(&mut registry, "curl", aliases_b, None);
        assert!(current_b.is_empty());
        let late_a = store.publish_index("curl", aliases_a, None, Arc::clone(&built_a));
        assert!(Arc::ptr_eq(&late_a, &built_a));
        assert!(!late_a.is_empty());
        let still_b = store.index_for(&mut registry, "curl", aliases_b, None);
        assert!(Arc::ptr_eq(&still_b, &current_b));
        assert_eq!(store.index.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn wrapped_commands_file_their_values_under_the_inner_spec() {
        let mut registry = registry_with(&[
            ("curl", CURL),
            (
                "sudo",
                r#"{
                  "names": ["sudo"],
                  "options": [{"names": ["-u"], "args": [{"name": "user"}]}],
                  "args": [{"name": "command", "isCommand": true}]
                }"#,
            ),
        ]);
        let store = HistoryStore::from_lines(vec![
            "sudo curl https://as-root.example".into(),
            "sudo -u www curl https://as-www.example".into(),
            "curl https://plain.example".into(),
        ]);
        let url = ArgSlot {
            root: "curl".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        let values = store.arg_values(&mut registry, None, None, &[url]);
        assert_eq!(
            values,
            vec![
                "https://plain.example",
                "https://as-www.example",
                "https://as-root.example"
            ]
        );

        // The command token itself is not a value of `sudo`'s argument.
        let sudo_command = ArgSlot {
            root: "sudo".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        let user = ArgSlot {
            option: Some("-u".into()),
            ..sudo_command.clone()
        };
        assert!(store.arg_values(&mut registry, None, None, &[sudo_command]).is_empty());
        assert_eq!(store.arg_values(&mut registry, None, None, &[user]), vec!["www"]);
    }

    #[test]
    fn variadic_values_past_the_argument_list_have_no_slot() {
        let mut registry = registry_with(&[(
            "scp",
            r#"{
              "names": ["scp"],
              "args": [
                {"name": "source", "isVariadic": true, "templates": ["history"]},
                {"name": "target", "templates": ["history"]}
              ]
            }"#,
        )]);
        let store = HistoryStore::from_lines(vec!["scp a b host:".into(), "scp only host2:".into()]);
        let slot = |index| ArgSlot {
            root: "scp".into(),
            path: Vec::new(),
            option: None,
            index,
        };
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[slot(0)]),
            vec!["only", "a"]
        );
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[slot(1)]),
            vec!["host2:", "b"]
        );
        assert!(store.arg_values(&mut registry, None, None, &[slot(2)]).is_empty());
    }

    #[test]
    fn evicted_history_index_rebuilds_and_a_hot_spec_stays_cached() {
        let mut owned = Vec::new();
        let mut lines = Vec::new();
        for index in 0..=MAX_CACHED_INDEXES {
            let name = format!("cmd{index}");
            owned.push((
                name.clone(),
                format!(r#"{{"names":["{name}"],"args":[{{"name":"value"}}]}}"#),
            ));
            lines.push(format!("{name} value{index}"));
        }
        let pairs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(name, spec)| (name.as_str(), spec.as_str()))
            .collect();
        let mut registry = registry_with(&pairs);
        let store = HistoryStore::from_lines(lines);

        let mut cached = Vec::new();
        for index in 0..=MAX_CACHED_INDEXES {
            cached.push(store.index_for(&mut registry, &format!("cmd{index}"), None, None));
        }
        let rebuilt = store.index_for(&mut registry, "cmd0", None, None);
        assert_eq!(rebuilt.as_ref(), cached[0].as_ref());
        assert!(
            !Arc::ptr_eq(&rebuilt, &cached[0]),
            "the evicted index is rebuilt instead of retained"
        );
        let hot = store.index_for(&mut registry, &format!("cmd{MAX_CACHED_INDEXES}"), None, None);
        assert!(Arc::ptr_eq(&hot, &cached[MAX_CACHED_INDEXES]));

        let slot = ArgSlot {
            root: "cmd0".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[slot]),
            vec!["value0".to_owned()]
        );
    }

    #[test]
    fn history_index_byte_budget_evicts_old_entries_and_serves_oversized_values() {
        let mut registry = registry_with(&[
            ("a", r#"{"names":["a"],"args":[{"name":"value"}]}"#),
            ("b", r#"{"names":["b"],"args":[{"name":"value"}]}"#),
            ("large", r#"{"names":["large"],"args":[{"name":"value"}]}"#),
        ]);
        let first = "a".repeat(MAX_INDEX_BYTES / 2);
        let second = "b".repeat(MAX_INDEX_BYTES / 2);
        let large = "x".repeat(MAX_INDEX_BYTES + 128);
        let store = HistoryStore::from_lines(vec![
            format!("a {first}"),
            format!("b {second}"),
            format!("large {large}"),
        ]);
        let a = store.index_for(&mut registry, "a", None, None);
        let b = store.index_for(&mut registry, "b", None, None);
        assert!(!a.is_empty() && !b.is_empty());
        {
            let cache = store.index.lock().unwrap();
            assert!(!cache.entries.contains_key("a"));
            assert!(cache.entries.contains_key("b"));
            assert!(cache.allocated_bytes <= MAX_INDEX_BYTES);
        }
        let index = store.index_for(&mut registry, "large", None, None);
        assert_eq!(index.values().next().unwrap(), &[large]);
        assert!(!store.index.lock().unwrap().entries.contains_key("large"));
        assert!(store.diagnostics().index_bytes <= MAX_INDEX_BYTES);
        let aliases = format!("alias irrelevant='{}'", "x".repeat(MAX_INDEX_BYTES));
        let uncached = store.index_for(&mut registry, "a", Some(&aliases), Some("zsh"));
        assert_eq!(uncached.values().next().unwrap(), &[first]);
        assert_eq!(store.diagnostics().index_count, 0);
        assert_eq!(
            store.diagnostics().index_bytes,
            0,
            "oversized contexts are not retained"
        );
        store.release_derived_indexes();
        assert_eq!(store.diagnostics().index_bytes, 0);
        assert_eq!(store.lines().len(), 3, "retirement preserves raw history");
    }

    #[test]
    fn history_that_enters_one_load_spec_does_not_load_the_sibling() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud.json"),
            r#"{
              "names":["gcloud"],
              "subcommands":[
                {"names":["sql"],"loadSpec":"gcloud/sql"},
                {"names":["compute"],"loadSpec":"gcloud/compute"}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/sql.json"),
            r#"{
              "names":["sql-target"],
              "args":[{"name":"instance","templates":["history"]}],
              "subcommands":[{"names":["backups"]}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/compute.json"),
            r#"{
              "names":["compute"],
              "args":[{"name":"name"}],
              "subcommands":[{"names":["instances"]}]
            }"#,
        )
        .unwrap();
        fs::write(dir.path().join("index.json"), r#"{"files":{"gcloud":"gcloud.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let _kept = dir.keep();
        {
            let gcloud = registry.get("gcloud").expect("gcloud");
            assert!(gcloud.find_subcommand("sql").expect("sql stub").args.is_empty());
            assert!(gcloud.find_subcommand("compute").expect("compute stub").args.is_empty());
        }
        let store = HistoryStore::from_lines(vec!["gcloud sql prod".into()]);
        let slot = ArgSlot {
            root: "gcloud".into(),
            path: vec!["sql".into()],
            option: None,
            index: 0,
        };
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[slot]),
            vec!["prod".to_owned()]
        );
        assert!(registry.cached_load_spec("gcloud/sql").is_some());
        assert!(registry.cached_load_spec("gcloud/compute").is_none());
    }
}
