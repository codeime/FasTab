//! Hands the heap's free pages back to the system once the app goes quiet.
//!
//! Retiring the overlay window, the UI caches and the engine's specs frees
//! their memory to malloc, not to the kernel. libmalloc keeps the emptied
//! pages mapped and dirty, and `phys_footprint` counts every one of them, so an
//! idle app stayed near its high-water mark: 37 MB hours after a session
//! against 14.6 MB for a fresh launch. Even that fresh launch carried ~4 MB of
//! free-but-dirty heap, 53% of the default zone in `vmmap --summary`.
//!
//! `malloc_zone_pressure_relief` returns pages that hold no live allocation.
//! It cannot release a page pinned by one surviving object, and it does not
//! touch what Metal, CoreText or AppKit cache inside the frameworks.
//!
//! Every engine request and every UI cache release counts as activity. The
//! relief runs once, [`QUIET`] after the last of them: later than the engine's
//! 10 s spec and hook grace and the overlay's 10 s window grace, so what those
//! retire is already back in malloc by then. A quiet app is never woken.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

const QUIET: Duration = Duration::from_secs(15);

static TRIMMER: LazyLock<Trimmer> = LazyLock::new(Trimmer::default);

/// Start the trimmer on the current tokio runtime. Startup counts as activity,
/// so the first relief collects what initialization left behind.
pub fn start() {
    note_activity();
    tokio::spawn(TRIMMER.run(QUIET, relieve));
}

/// Cheap enough for every keystroke: an atomic add and a stored permit.
pub fn note_activity() {
    TRIMMER.note();
}

#[derive(Default)]
struct Trimmer {
    activity: AtomicU64,
    wake: Notify,
}

impl Trimmer {
    fn note(&self) {
        self.activity.fetch_add(1, Ordering::Relaxed);
        self.wake.notify_one();
    }

    async fn run(&self, quiet: Duration, mut relieve: impl FnMut()) {
        let mut relieved = None;
        loop {
            self.wake.notified().await;
            // Notes made while the last quiet period was running leave a
            // permit behind. Without new activity there is nothing new to
            // collect, so that permit must not buy a second relief.
            let mut seen = self.activity.load(Ordering::Relaxed);
            if relieved == Some(seen) {
                continue;
            }
            loop {
                tokio::time::sleep(quiet).await;
                let now = self.activity.load(Ordering::Relaxed);
                if now == seen {
                    break;
                }
                seen = now;
            }
            relieve();
            relieved = Some(seen);
        }
    }
}

#[cfg(target_os = "macos")]
fn relieve() {
    unsafe extern "C" {
        /// `<malloc/malloc.h>`. A null zone means every registered zone; a zero
        /// goal means release as much as possible. Returns the bytes released.
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: libmalloc takes each zone's lock itself; it makes this same call
    // from its memory-pressure handler on an arbitrary thread.
    let released = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    tracing::debug!(released, "Returned free heap pages to the system");
}

#[cfg(not(target_os = "macos"))]
fn relieve() {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::Trimmer;

    const QUIET: Duration = Duration::from_millis(200);

    fn spawn_counting(trimmer: &Arc<Trimmer>) -> Arc<AtomicUsize> {
        let reliefs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&reliefs);
        let trimmer = Arc::clone(trimmer);
        tokio::spawn(async move {
            trimmer
                .run(QUIET, move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                })
                .await;
        });
        reliefs
    }

    #[tokio::test]
    async fn relieves_once_per_quiet_period_and_never_while_busy() {
        let trimmer = Arc::new(Trimmer::default());
        let reliefs = spawn_counting(&trimmer);

        for _ in 0..30 {
            trimmer.note();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            reliefs.load(Ordering::SeqCst),
            0,
            "activity keeps postponing the relief"
        );

        tokio::time::sleep(QUIET * 3).await;
        assert_eq!(reliefs.load(Ordering::SeqCst), 1);

        // The permits left by the busy notes must not buy a second relief.
        tokio::time::sleep(QUIET * 3).await;
        assert_eq!(reliefs.load(Ordering::SeqCst), 1, "an idle app is left alone");

        trimmer.note();
        tokio::time::sleep(QUIET * 3).await;
        assert_eq!(reliefs.load(Ordering::SeqCst), 2, "new activity earns a new relief");
    }

    #[tokio::test]
    async fn activity_before_the_task_runs_is_not_lost() {
        let trimmer = Arc::new(Trimmer::default());
        trimmer.note();
        let reliefs = spawn_counting(&trimmer);

        tokio::time::sleep(QUIET * 3).await;
        assert_eq!(reliefs.load(Ordering::SeqCst), 1);
    }
}
