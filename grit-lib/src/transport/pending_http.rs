//! Run blocking HTTP client calls on a worker thread so the caller can poll
//! [`Cancel`] while the socket would otherwise block (discovery GET, upload-pack
//! POST setup, receive-pack report).

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::progress::{mirror_stack_cancel, Cancel, CancelSignal};

/// How often a wait for an HTTP worker checks for cancellation.
pub(crate) const HTTP_CANCEL_POLL: Duration = Duration::from_millis(20);

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
    pub(crate) fn wait(&self, cancel: &dyn Cancel) -> Result<T> {
        self.wait_with_stack(cancel, None, None)
    }

    /// Like [`Self::wait`], but also mirrors `stack` into `stack_watch` on the owning thread.
    pub(crate) fn wait_with_stack(
        &self,
        cancel: &dyn Cancel,
        stack: Option<&dyn Cancel>,
        stack_watch: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<T> {
        cancel.poll_before_wait();
        let signal = CancelSignal::new(cancel);
        loop {
            if let (Some(stack), Some(watch)) = (stack, stack_watch) {
                mirror_stack_cancel(stack, watch);
            }
            if stack_watch.is_some_and(|w| w.load(std::sync::atomic::Ordering::Relaxed)) {
                return Err(Error::Cancelled);
            }
            match self.0.recv_timeout(HTTP_CANCEL_POLL) {
                Ok(response) => return response,
                Err(RecvTimeoutError::Timeout) => signal.poll()?,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Message("HTTP worker thread panicked".to_owned()));
                }
            }
        }
    }
}
