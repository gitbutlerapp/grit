//! Run blocking HTTP client calls on a worker thread so the caller can poll
//! [`CancelToken`] while the socket would otherwise block (discovery GET, upload-pack
//! POST setup, receive-pack report).

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::progress::{mirror_stack_cancel, CancelSignal, CancelToken};
use crate::remote::RemoteError;

fn remote_err(err: RemoteError) -> Error {
    Error::Remote(err)
}

/// How often a wait for an HTTP worker checks for cancellation.
pub(crate) const HTTP_CANCEL_POLL: Duration = Duration::from_millis(20);

/// Git-shaped low-speed abort message for silent-socket waits on worker threads.
pub(crate) const LOW_SPEED_STALL_MSG: &str =
    "HTTP transfer speed below http.lowSpeedLimit for http.lowSpeedTime";

/// Whether `msg` is (or wraps) a low-speed transport failure from HTTP.
pub(crate) fn is_low_speed_transport_message(msg: &str) -> bool {
    msg.contains(LOW_SPEED_STALL_MSG)
}

/// Run blocking HTTP work on a worker thread while polling `cancel` on the caller.
pub(crate) fn run_interruptible<T, F>(
    thread_name: &str,
    cancel: &CancelToken,
    low_speed_window: Option<Duration>,
    work: F,
) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    PendingHttp::spawn(thread_name, work)?.wait_with_watch(cancel, None, None, low_speed_window)
}

/// A blocking HTTP operation running on its own thread.
pub(crate) struct PendingHttp<T>(Receiver<Result<T>>);

impl<T: Send + 'static> PendingHttp<T> {
    /// Spawn `work` on a named background thread and return a handle to its result.
    pub(crate) fn spawn<F>(thread_name: &str, work: F) -> std::io::Result<Self>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                let _ = tx.send(work());
            })?;
        Ok(Self(rx))
    }

    /// The result, or [`Error::Cancelled`] once `cancel` fires first. A cancelled
    /// request is left to finish on its own thread.
    pub(crate) fn wait(&self, cancel: &CancelToken) -> Result<T> {
        self.wait_with_watch(cancel, None, None, None)
    }

    /// Like [`Self::wait`], but mirrors `owner` into `stack_watch` on each poll
    /// (when both are set) and checks the shared watch flag.
    ///
    /// When `low_speed_window` is set, this loop applies a whole-request deadline for
    /// worker threads that may block without body-read progress (for example bundle URI
    /// probes via [`run_interruptible`]). Discovery GETs that read the body on the worker
    /// with [`crate::transport::http::ureq_client::LowSpeedReader`] must pass `None` so
    /// low-speed is enforced on bytes, not wall clock alone.
    pub(crate) fn wait_with_watch(
        &self,
        cancel: &CancelToken,
        owner: Option<&CancelToken>,
        stack_watch: Option<&std::sync::atomic::AtomicBool>,
        low_speed_window: Option<Duration>,
    ) -> Result<T> {
        cancel.poll_before_wait();
        let signal = CancelSignal::new(cancel);
        let started = Instant::now();
        loop {
            if let (Some(owner), Some(watch)) = (owner, stack_watch) {
                mirror_stack_cancel(owner, watch);
            }
            if stack_watch.is_some_and(|w| w.load(std::sync::atomic::Ordering::Relaxed)) {
                return Err(Error::Cancelled);
            }
            if low_speed_window.is_some_and(|window| started.elapsed() >= window) {
                return Err(remote_err(RemoteError::HttpLowSpeedStall(
                    LOW_SPEED_STALL_MSG.to_owned(),
                )));
            }
            match self.0.recv_timeout(HTTP_CANCEL_POLL) {
                Ok(response) => return response,
                Err(RecvTimeoutError::Timeout) => signal.poll()?,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(remote_err(RemoteError::HttpWorkerPanicked));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Instant;

    /// Regression helper: the faulty #2469 follow-up treated `http.lowSpeedTime` as a
    /// whole-request deadline on the wait loop (see `started.elapsed() >= window`).
    fn wall_clock_low_speed_wait<T>(
        rx: &Receiver<Result<T>>,
        low_speed_window: Duration,
        cancel: &CancelToken,
    ) -> Result<T> {
        cancel.poll_before_wait();
        let signal = CancelSignal::new(cancel);
        let started = Instant::now();
        loop {
            if started.elapsed() >= low_speed_window {
                return Err(remote_err(RemoteError::HttpLowSpeedStall(
                    LOW_SPEED_STALL_MSG.to_owned(),
                )));
            }
            match rx.recv_timeout(HTTP_CANCEL_POLL) {
                Ok(response) => return response,
                Err(RecvTimeoutError::Timeout) => signal.poll()?,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(remote_err(RemoteError::HttpWorkerPanicked));
                }
            }
        }
    }

    #[test]
    fn wall_clock_low_speed_wait_aborts_before_slow_worker_finishes() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1900));
            let _ = tx.send(Ok(()));
        });
        let started = Instant::now();
        let err = wall_clock_low_speed_wait(&rx, Duration::from_secs(1), &CancelToken::never())
            .expect_err("wall-clock low-speed must abort before the worker finishes");
        assert!(
            matches!(
                &err,
                Error::Remote(RemoteError::HttpLowSpeedStall(msg)) if msg == LOW_SPEED_STALL_MSG
            ),
            "unexpected error: {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "abort took {:?}, expected ~1s",
            started.elapsed()
        );
    }

    #[test]
    fn wait_with_watch_outlasts_low_speed_window_for_slow_worker() {
        let pending = PendingHttp::spawn("grit-http-get-test", move || {
            std::thread::sleep(Duration::from_millis(1900));
            Ok(42_u8)
        })
        .expect("spawn");
        let started = Instant::now();
        assert_eq!(pending.wait(&CancelToken::never()).expect("wait"), 42);
        assert!(
            started.elapsed() >= Duration::from_millis(1500),
            "expected ~1.9s worker, got {:?}",
            started.elapsed()
        );
    }
}
