//! Reports the shell's exit to the main loop.
//!
//! fterm used to park a thread in `waitpid` for the life of every tab. On
//! macOS `fastab_util::process_exit` (a kqueue `NOTE_EXIT` registration polled
//! through tokio's reactor) does the same wait with no thread, and the shell is
//! reaped once it fires. The thread is still the fallback, and the only path
//! elsewhere.

use tokio::sync::oneshot;

pub type Shell = Box<dyn portable_pty::Child + Send + Sync>;
pub type ExitSender = oneshot::Sender<std::io::Result<portable_pty::ExitStatus>>;

/// Sends the shell's exit status on `exited` once it has exited. Call inside
/// the tokio runtime.
pub fn watch(shell: Shell, exited: ExitSender) {
    #[cfg(target_os = "macos")]
    let Some((shell, exited)) = kqueue::watch(shell, exited) else {
        return;
    };
    wait_on_thread(shell, exited);
}

fn wait_on_thread(mut shell: Shell, exited: ExitSender) {
    std::thread::spawn(move || exited.send(shell.wait()));
}

#[cfg(target_os = "macos")]
mod kqueue {
    use fastab_util::process_exit::ExitWatch;
    use tracing::warn;

    use super::{ExitSender, Shell};

    /// `None` once the watch owns the shell; otherwise both come back for the
    /// thread to wait on.
    pub(super) fn watch(mut shell: Shell, exited: ExitSender) -> Option<(Shell, ExitSender)> {
        let Some(pid) = shell.process_id().and_then(|pid| i32::try_from(pid).ok()) else {
            return Some((shell, exited));
        };
        let watch = match ExitWatch::new(&[pid]) {
            Ok(watch) => watch,
            Err(error) => {
                warn!(%error, "Could not watch the shell's exit; waiting on a thread");
                return Some((shell, exited));
            },
        };
        // A shell that died before the registration was made never fires it.
        // Reap that one now; one still alive will fire the event when it exits.
        match shell.try_wait() {
            Ok(Some(status)) => {
                let _ = exited.send(Ok(status));
                return None;
            },
            Ok(None) => {},
            Err(error) => {
                let _ = exited.send(Err(error));
                return None;
            },
        }
        tokio::spawn(async move {
            if let Err(error) = watch.exited().await {
                warn!(%error, "Lost the shell's exit watch; waiting on a thread");
                super::wait_on_thread(shell, exited);
                return;
            }
            // NOTE_EXIT has fired, so this reaps the shell without a long block.
            let _ = exited.send(shell.wait());
        });
        None
    }

    #[cfg(test)]
    mod tests {
        use std::process::{Command, Stdio};
        use std::time::Duration;

        use tokio::sync::oneshot;

        // The kqueue wait itself is tested with `fastab_util::process_exit`.

        fn spawn_sleep(seconds: &str) -> std::process::Child {
            Command::new("/bin/sleep")
                .arg(seconds)
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn sleep")
        }

        /// The whole path the main loop relies on: the status arrives once the
        /// shell exits, and no thread was needed to wait for it.
        #[tokio::test]
        async fn the_exit_status_reaches_the_main_loop() {
            let (exited, receiver) = oneshot::channel();
            super::super::watch(Box::new(spawn_sleep("0.2")), exited);
            let status = tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .expect("status before the timeout")
                .expect("sender kept until the exit")
                .expect("shell reaped");
            assert!(status.success());
        }

        /// A shell that is already a zombie when the watch is made never fires
        /// NOTE_EXIT; `try_wait` reports it instead.
        #[tokio::test]
        async fn a_shell_that_exited_before_the_watch_is_still_reported() {
            let child = spawn_sleep("0");
            std::thread::sleep(Duration::from_millis(200));
            let (exited, receiver) = oneshot::channel();
            super::super::watch(Box::new(child), exited);
            let status = tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .expect("status without waiting for an event")
                .expect("sender kept until the exit")
                .expect("shell reaped");
            assert!(status.success());
        }
    }
}
