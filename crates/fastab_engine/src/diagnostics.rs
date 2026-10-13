//! Numeric resource snapshots. Counters are cumulative for one Engine lifetime.

/// One hook cache map, measured without expiring entries or exposing keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheMapDiagnostics {
    pub entries: usize,
    /// Estimated retained payload bytes, not allocator usage or process footprint.
    /// Counts owned string/vector capacity for script and suggestion results;
    /// specs use the registry's tree estimate. Omits map nodes and allocator
    /// overhead. Shared option Arcs can be counted across separate spec entries.
    pub allocated_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    /// Also included in `misses`; expiration uses the reading caller's TTL.
    pub expired_removals: u64,
    /// Whole-map clears at capacity, excluding explicit clears and replacement.
    pub capacity_clears: u64,
}

/// Independently locked snapshots of the three per-engine hook maps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookDiagnostics {
    pub suggestions: CacheMapDiagnostics,
    pub script_output: CacheMapDiagnostics,
    pub specs: CacheMapDiagnostics,
}

/// Native catalog ownership and deduplicated descriptor counts; no parsing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookCatalogDiagnostics {
    /// Distinguishes an idle/unloaded owner from a missing or rejected sidecar.
    pub load_attempted: bool,
    pub loaded: bool,
    pub typed_entries: usize,
    pub adapter_entries: usize,
    pub descriptor_count: usize,
    pub parsed_descriptor_count: usize,
    /// Retained one-shot sidecar allocation from snapshot hashing, not process footprint.
    #[serde(default)]
    pub captured_bytes: usize,
}

/// Parsed file resources; counts deduplicate paths, bytes deduplicate live trees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegistryDiagnostics {
    /// Unpinned file paths participating in idle release (aliases count once).
    pub cached_file_count: usize,
    pub idle_file_count: usize,
    /// Parsed tree payload estimate, excluding indexes and allocator overhead.
    pub allocated_bytes: usize,
    pub next_deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryDiagnostics {
    pub line_count: usize,
    pub index_count: usize,
    /// History lines, derived indexes and NextWords payload estimate, not footprint.
    pub allocated_bytes: usize,
    #[serde(default)]
    pub index_bytes: usize,
    #[serde(default)]
    pub next_word_nodes: usize,
    #[serde(default)]
    pub next_word_entries: usize,
    #[serde(default)]
    pub next_word_bytes: usize,
}

/// Rows retained for generator trigger/debounce refiltering in the active input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GeneratorSessionDiagnostics {
    pub generators: usize,
    pub rows: usize,
    /// Owned string/vector capacity estimate, excluding allocator overhead.
    pub allocated_bytes: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EngineDiagnostics {
    pub registry: RegistryDiagnostics,
    pub hooks: HookDiagnostics,
    #[serde(default)]
    pub hook_catalog: HookCatalogDiagnostics,
    pub history: HistoryDiagnostics,
    #[serde(default)]
    pub generator_session: GeneratorSessionDiagnostics,
}

/// Worker lifetime counters. `cancelled` includes jobs skipped before execution;
/// `failed` includes initialization failures, watchdog timeouts and panics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestDiagnostics {
    pub submitted: u64,
    pub started: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub failed: u64,
    pub engine_initializations: u64,
    /// Includes supervised initialization and cache-refresh operations.
    pub watchdog_timeouts: u64,
    /// Includes supervised initialization and cache-refresh operations.
    pub panics: u64,
}

/// Mailbox payload is an estimate of owned capacities, not physical footprint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerResourceDiagnostics {
    pub queued_jobs: usize,
    pub queued_payload_bytes: usize,
    pub active_operations: usize,
    pub abandoned_operations: usize,
}

/// Read-only host counters, collected without constructing a window or asset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostResourceDiagnostics {
    #[serde(default)]
    pub host_async_events: usize,
    #[serde(default)]
    pub host_async_payload_bytes: usize,
    #[serde(default)]
    pub host_async_waiters: usize,
    #[serde(default)]
    pub host_async_waiting_bytes: usize,
    #[serde(default)]
    pub host_sync_events: usize,
    #[serde(default)]
    pub host_sync_payload_bytes: usize,
    #[serde(default)]
    pub host_ax_events: usize,
    #[serde(default)]
    pub ipc_remote_connections: usize,
    #[serde(default)]
    pub ipc_local_connections: usize,
    pub gpui_windows: usize,
    pub overlay_visible: bool,
    pub file_icon_paths: usize,
    pub file_icon_images: usize,
    pub file_icon_worker_active: bool,
    pub file_icon_pending_paths: usize,
    pub ipc_sessions: usize,
    #[serde(default)]
    pub ipc_session_edit_bytes: usize,
    #[serde(default)]
    pub ipc_session_edit_capacity: usize,
    pub ipc_pending_messages: usize,
    pub ipc_pending_bytes: usize,
    pub ipc_pending_responses: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EngineClientDiagnostics {
    pub engine: Option<EngineDiagnostics>,
    pub requests: RequestDiagnostics,
    #[serde(default)]
    pub worker: WorkerResourceDiagnostics,
    #[serde(default)]
    pub host: Option<HostResourceDiagnostics>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn engine_snapshots_without_catalog_counts_still_deserialize() {
        let mut legacy = serde_json::to_value(super::EngineDiagnostics::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("hook_catalog");
        legacy.as_object_mut().unwrap().remove("generator_session");
        let restored: super::EngineDiagnostics = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.hook_catalog, Default::default());
        assert_eq!(restored.generator_session, Default::default());
    }

    #[test]
    fn catalog_snapshots_without_captured_bytes_still_deserialize() {
        let mut legacy = serde_json::to_value(super::HookCatalogDiagnostics::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("captured_bytes");
        let restored: super::HookCatalogDiagnostics = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.captured_bytes, 0);
    }

    #[test]
    fn history_snapshots_without_derived_resource_counts_still_deserialize() {
        let restored: super::HistoryDiagnostics = serde_json::from_value(serde_json::json!({
            "line_count": 3, "index_count": 1, "allocated_bytes": 256
        }))
        .unwrap();
        assert_eq!(restored.index_bytes, 0);
        assert_eq!(restored.next_word_bytes, 0);
        assert_eq!(restored.next_word_nodes, 0);
        assert_eq!(restored.next_word_entries, 0);
    }
}
