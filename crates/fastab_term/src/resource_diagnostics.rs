//! Numeric observations at input/parse/flush boundaries. These independent
//! atomics are an approximate snapshot, not an allocator or footprint measure.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static INPUT_OUTER_BYTES: AtomicUsize = AtomicUsize::new(0);
static INPUT_OUTER_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static PARSER_READ_BYTES: AtomicUsize = AtomicUsize::new(0);
static PARSER_READ_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static PARSER_RAW_BYTES: AtomicUsize = AtomicUsize::new(0);
// BytesMut view capacity cannot describe a shared allocation's full backing.
static PARSER_RAW_VIEW_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_PTY_BYTES: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_PTY_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_INSERTS: AtomicUsize = AtomicUsize::new(0);
static SYNC_OUTPUT_BYTES: AtomicUsize = AtomicUsize::new(0);
static SYNC_OUTPUT_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static SYNC_OUTPUT_DEADLINE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn input_outer(bytes: usize, capacity: usize) {
    INPUT_OUTER_BYTES.store(bytes, Ordering::Relaxed);
    INPUT_OUTER_CAPACITY.store(capacity, Ordering::Relaxed);
}

pub(crate) fn parser(read_bytes: usize, read_capacity: usize, raw_bytes: usize, raw_view_capacity: usize) {
    PARSER_READ_BYTES.store(read_bytes, Ordering::Relaxed);
    PARSER_READ_CAPACITY.store(read_capacity, Ordering::Relaxed);
    PARSER_RAW_BYTES.store(raw_bytes, Ordering::Relaxed);
    PARSER_RAW_VIEW_CAPACITY.store(raw_view_capacity, Ordering::Relaxed);
}

pub(crate) fn deferred(bytes: usize, capacity: usize, inserts: usize) {
    DEFERRED_PTY_BYTES.store(bytes, Ordering::Relaxed);
    DEFERRED_PTY_CAPACITY.store(capacity, Ordering::Relaxed);
    DEFERRED_INSERTS.store(inserts, Ordering::Relaxed);
}

pub(crate) fn sync_output(bytes: usize, capacity: usize, deadline_active: bool) {
    SYNC_OUTPUT_BYTES.store(bytes, Ordering::Relaxed);
    SYNC_OUTPUT_CAPACITY.store(capacity, Ordering::Relaxed);
    SYNC_OUTPUT_DEADLINE_ACTIVE.store(deadline_active, Ordering::Relaxed);
}

pub(crate) fn json() -> String {
    format!(
        concat!(
            "{{\"input_outer_bytes\":{},\"input_outer_capacity\":{},",
            "\"parser_read_bytes\":{},\"parser_read_capacity\":{},",
            "\"parser_raw_bytes\":{},\"parser_raw_view_capacity\":{},",
            "\"deferred_pty_bytes\":{},\"deferred_pty_capacity\":{},",
            "\"deferred_inserts\":{},\"sync_output_bytes\":{},",
            "\"sync_output_capacity\":{},\"sync_output_deadline_active\":{},\"incoming\":{}}}"
        ),
        INPUT_OUTER_BYTES.load(Ordering::Relaxed),
        INPUT_OUTER_CAPACITY.load(Ordering::Relaxed),
        PARSER_READ_BYTES.load(Ordering::Relaxed),
        PARSER_READ_CAPACITY.load(Ordering::Relaxed),
        PARSER_RAW_BYTES.load(Ordering::Relaxed),
        PARSER_RAW_VIEW_CAPACITY.load(Ordering::Relaxed),
        DEFERRED_PTY_BYTES.load(Ordering::Relaxed),
        DEFERRED_PTY_CAPACITY.load(Ordering::Relaxed),
        DEFERRED_INSERTS.load(Ordering::Relaxed),
        SYNC_OUTPUT_BYTES.load(Ordering::Relaxed),
        SYNC_OUTPUT_CAPACITY.load(Ordering::Relaxed),
        SYNC_OUTPUT_DEADLINE_ACTIVE.load(Ordering::Relaxed),
        crate::ipc::incoming::diagnostics_json(),
    )
}
