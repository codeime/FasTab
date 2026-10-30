//! Keeps a chosen input method running for as long as the desktop app runs.
//!
//! The desktop restores the IME once at launch. That check can run while an
//! IME the previous desktop launched is still alive and about to be killed by
//! loginwindow (see `fastab_integrations::input_method::launch_without_parent_app`):
//! it saw a live helper with the right hash, left it alone, and a second later
//! nothing was serving terminals. A crash or a stray kill had the same result
//! until the next launch. This watches the running IME and restores it again
//! when it exits.
//!
//! The wait is `fastab_util::process_exit`: a kqueue `NOTE_EXIT` registration
//! polled through tokio's reactor, so no thread and no wakeups while the IME
//! stays up.

use std::io;
use std::time::{Duration, Instant};

use fastab_integrations::input_method::InputMethod;
use fastab_util::process_exit::ExitWatch;
use tracing::{debug, info, warn};

/// Let whoever stopped the IME finish first: an install replaces the process
/// under the operation lock, and loginwindow's teardown takes a few seconds.
const SETTLE: Duration = Duration::from_secs(2);
/// While the IME is not chosen, nothing is watched; check back this rarely.
const NOT_CHOSEN_RECHECK: Duration = Duration::from_secs(300);
/// A helper that keeps dying is not relaunched in a loop.
const RELAUNCH_WINDOW: Duration = Duration::from_secs(300);
const MAX_RELAUNCHES_PER_WINDOW: usize = 3;

pub async fn run(input_method: InputMethod) {
    let mut budget = RelaunchBudget::default();
    loop {
        let pids = input_method.running_process_ids();
        if pids.is_empty() {
            match input_method.should_restore_on_launch() {
                Ok(true) => {},
                Ok(false) => {
                    tokio::time::sleep(NOT_CHOSEN_RECHECK).await;
                    continue;
                },
                Err(err) => {
                    warn!(%err, "Could not read whether to keep the input method running");
                    tokio::time::sleep(NOT_CHOSEN_RECHECK).await;
                    continue;
                },
            }
            if !budget.try_take(Instant::now()) {
                warn!("The input method keeps exiting; pausing relaunches");
                tokio::time::sleep(RELAUNCH_WINDOW).await;
                continue;
            }
            match input_method.restore_if_chosen().await {
                Ok(true) => info!("Relaunched the input method after it exited"),
                Ok(false) => {},
                Err(err) => warn!(%err, "Failed to relaunch the input method"),
            }
            tokio::time::sleep(SETTLE).await;
            continue;
        }

        if let Err(err) = wait_for_any_exit(&pids).await {
            warn!(%err, "Could not watch the input method process");
            tokio::time::sleep(NOT_CHOSEN_RECHECK).await;
            continue;
        }
        debug!(?pids, "Input method exited");
        tokio::time::sleep(SETTLE).await;
    }
}

/// At most [`MAX_RELAUNCHES_PER_WINDOW`] relaunches in any [`RELAUNCH_WINDOW`].
#[derive(Default)]
struct RelaunchBudget {
    recent: Vec<Instant>,
}

impl RelaunchBudget {
    fn try_take(&mut self, now: Instant) -> bool {
        self.recent
            .retain(|at| now.saturating_duration_since(*at) < RELAUNCH_WINDOW);
        if self.recent.len() >= MAX_RELAUNCHES_PER_WINDOW {
            return false;
        }
        self.recent.push(now);
        true
    }
}

/// Resolves when any of `pids` exits, at once if one already has.
async fn wait_for_any_exit(pids: &[i32]) -> io::Result<()> {
    ExitWatch::new(pids)?.exited().await
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{MAX_RELAUNCHES_PER_WINDOW, RELAUNCH_WINDOW, RelaunchBudget};

    // The exit wait itself is tested with `fastab_util::process_exit`.

    #[test]
    fn relaunches_are_capped_per_window() {
        let mut budget = RelaunchBudget::default();
        let start = Instant::now();
        for _ in 0..MAX_RELAUNCHES_PER_WINDOW {
            assert!(budget.try_take(start));
        }
        assert!(!budget.try_take(start + Duration::from_secs(1)));
        assert!(budget.try_take(start + RELAUNCH_WINDOW));
    }
}
