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
//! The wait is a kqueue `EVFILT_PROC`/`NOTE_EXIT` registration polled through
//! tokio's reactor: no thread, and no wakeups while the IME stays up.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use fastab_integrations::input_method::InputMethod;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
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
    let Some(queue) = ProcessExitQueue::watch(pids)? else {
        return Ok(());
    };
    let queue = AsyncFd::with_interest(queue, Interest::READABLE)?;
    // A kqueue descriptor turns readable once one of its events has fired.
    let _ready = queue.readable().await?;
    Ok(())
}

struct ProcessExitQueue(OwnedFd);

impl AsRawFd for ProcessExitQueue {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl ProcessExitQueue {
    /// `None` when one of `pids` has already exited.
    fn watch(pids: &[i32]) -> io::Result<Option<Self>> {
        // SAFETY: kqueue takes no arguments; a negative result is an error.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor we just created and nobody else owns.
        let queue = Self(unsafe { OwnedFd::from_raw_fd(raw) });
        for &pid in pids {
            let Ok(ident) = libc::uintptr_t::try_from(pid) else {
                continue;
            };
            let change = libc::kevent {
                ident,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: one valid change record, no output buffer, no timeout.
            let result = unsafe { libc::kevent(raw, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(None);
                }
                return Err(error);
            }
        }
        Ok(Some(queue))
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{MAX_RELAUNCHES_PER_WINDOW, RELAUNCH_WINDOW, RelaunchBudget, wait_for_any_exit};

    fn spawn_sleep(seconds: &str) -> std::process::Child {
        Command::new("/bin/sleep")
            .arg(seconds)
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn sleep")
    }

    fn pid_of(child: &std::process::Child) -> i32 {
        i32::try_from(child.id()).expect("pid fits")
    }

    #[tokio::test]
    async fn an_exit_wakes_the_wait() {
        let mut child = spawn_sleep("0.2");
        let pid = pid_of(&child);
        tokio::time::timeout(Duration::from_secs(5), wait_for_any_exit(&[pid]))
            .await
            .expect("exit observed before the timeout")
            .expect("watch registered");
        child.wait().expect("reap");
    }

    #[tokio::test]
    async fn a_live_process_keeps_the_wait_pending() {
        let mut child = spawn_sleep("30");
        let pid = pid_of(&child);
        let pending = tokio::time::timeout(Duration::from_millis(300), wait_for_any_exit(&[pid])).await;
        assert!(pending.is_err(), "a running process must not look exited");
        child.kill().expect("kill");
        child.wait().expect("reap");
    }

    #[tokio::test]
    async fn an_already_gone_process_returns_at_once() {
        let mut child = spawn_sleep("0");
        let pid = pid_of(&child);
        child.wait().expect("reap");
        tokio::time::timeout(Duration::from_secs(1), wait_for_any_exit(&[pid]))
            .await
            .expect("no wait for a process that is gone")
            .expect("ESRCH is not an error");
    }

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
