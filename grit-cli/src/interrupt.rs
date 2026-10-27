//! Ctrl+C as a library [`Cancel`] signal.
//!
//! Without a handler `SIGINT` kills the process mid-write, so library rollbacks never run.
//! While an [`InterruptCancel`] is installed the first `SIGINT` only asks the running
//! operation to stop (it then undoes its partial work and returns `Error::Cancelled`); the
//! handler resets itself, so a second Ctrl+C terminates the process as usual.

use std::sync::atomic::{AtomicBool, Ordering};

use grit_lib::progress::Cancel;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Cancels the running library operation on the first Ctrl+C; restores the previous handler
/// when dropped.
pub struct InterruptCancel {
    #[cfg(unix)]
    previous: libc::sigaction,
}

#[cfg(unix)]
extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

impl InterruptCancel {
    /// Route the next `SIGINT` to [`Cancel::is_cancelled`] instead of terminating.
    #[cfg(unix)]
    pub fn install() -> Self {
        INTERRUPTED.store(false, Ordering::Relaxed);
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe, and both
        // `sigaction` structs are fully initialized before the call.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESETHAND;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, &action, &mut previous);
            Self { previous }
        }
    }

    /// Ctrl+C keeps its default meaning on platforms without `SIGINT` handling here.
    #[cfg(not(unix))]
    pub fn install() -> Self {
        Self {}
    }
}

impl Cancel for InterruptCancel {
    fn is_cancelled(&self) -> bool {
        INTERRUPTED.load(Ordering::Relaxed)
    }
}

impl Drop for InterruptCancel {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: restores the action saved by `install`.
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous, std::ptr::null_mut());
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn current_sigint_handler() -> libc::sighandler_t {
        // SAFETY: querying the current action with a null new action.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current);
            current.sa_sigaction
        }
    }

    #[test]
    fn first_sigint_cancels_instead_of_terminating_and_drop_restores_the_handler() {
        let before = current_sigint_handler();
        let cancel = InterruptCancel::install();
        assert!(!cancel.is_cancelled());
        // SAFETY: the installed handler only sets an atomic flag.
        unsafe {
            libc::raise(libc::SIGINT);
        }
        assert!(cancel.is_cancelled());
        assert_eq!(
            current_sigint_handler(),
            libc::SIG_DFL,
            "a second Ctrl+C must terminate"
        );
        drop(cancel);
        assert_eq!(current_sigint_handler(), before);
    }
}
