//! Waits for processes to exit on tokio's reactor, without a thread.
//!
//! One kqueue `EVFILT_PROC`/`NOTE_EXIT` registration per pid, on a kqueue
//! descriptor that tokio polls. The desktop watches the input method with it
//! (`ime_watch`), and fterm the shell it runs (`child_exit`); nothing wakes
//! while the processes stay up.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Resolves when the first of its processes exits, or at once when one was
/// already gone as it was registered.
pub struct ExitWatch(Option<AsyncFd<Queue>>);

struct Queue(OwnedFd);

impl AsRawFd for Queue {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl ExitWatch {
    /// Registers every pid. Must be called inside a tokio runtime, with at
    /// least one pid: an empty watch never resolves.
    pub fn new(pids: &[i32]) -> io::Result<Self> {
        // SAFETY: kqueue takes no arguments; a negative result is an error.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor we just created and nobody else owns.
        let queue = Queue(unsafe { OwnedFd::from_raw_fd(raw) });
        for &pid in pids {
            let Ok(ident) = libc::uintptr_t::try_from(pid) else {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
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
                    return Ok(Self(None));
                }
                return Err(error);
            }
        }
        Ok(Self(Some(AsyncFd::with_interest(queue, Interest::READABLE)?)))
    }

    /// Resolves once one of the processes has exited.
    pub async fn exited(&self) -> io::Result<()> {
        if let Some(queue) = &self.0 {
            // A kqueue descriptor turns readable once one of its events fired.
            let _ready = queue.readable().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::ExitWatch;

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
    async fn an_exit_wakes_the_watch() {
        let mut child = spawn_sleep("0.2");
        let watch = ExitWatch::new(&[pid_of(&child)]).expect("watch registered");
        tokio::time::timeout(Duration::from_secs(5), watch.exited())
            .await
            .expect("exit observed before the timeout")
            .expect("watch polled");
        child.wait().expect("reap");
    }

    #[tokio::test]
    async fn a_live_process_keeps_the_watch_pending() {
        let mut child = spawn_sleep("30");
        let watch = ExitWatch::new(&[pid_of(&child)]).expect("watch registered");
        let pending = tokio::time::timeout(Duration::from_millis(300), watch.exited()).await;
        assert!(pending.is_err(), "a running process must not look exited");
        child.kill().expect("kill");
        child.wait().expect("reap");
    }

    #[tokio::test]
    async fn a_reaped_process_resolves_at_once() {
        let mut child = spawn_sleep("0");
        child.wait().expect("reap");
        let watch = ExitWatch::new(&[pid_of(&child)]).expect("ESRCH is not an error");
        tokio::time::timeout(Duration::from_secs(1), watch.exited())
            .await
            .expect("no wait for a process that is gone")
            .expect("watch polled");
    }

    #[tokio::test]
    async fn the_first_of_several_exits_wakes_the_watch() {
        let mut long = spawn_sleep("30");
        let mut short = spawn_sleep("0.2");
        let watch = ExitWatch::new(&[pid_of(&long), pid_of(&short)]).expect("watch registered");
        tokio::time::timeout(Duration::from_secs(5), watch.exited())
            .await
            .expect("the short process's exit is enough")
            .expect("watch polled");
        short.wait().expect("reap");
        long.kill().expect("kill");
        long.wait().expect("reap");
    }
}
