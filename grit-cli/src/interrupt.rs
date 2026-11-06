//! Cancel signals as a library [`CancelToken`] handle.
//!
//! Without handlers, `SIGINT`, `SIGTERM`, and `SIGHUP` on Unix (or `CTRL_C_EVENT` /
//! `CTRL_BREAK_EVENT` on a Windows console) kill the process mid-write, so library rollbacks
//! never run. While an [`InterruptCancel`] is installed the first such signal only asks the
//! running operation to stop (it then undoes its partial work and returns `Error::Cancelled`).
//! A second signal terminates the process as usual, unless it arrives within
//! [`DUPLICATE_WINDOW_NS`] of the first: tools such as `timeout` signal both the child and
//! its process group, and that duplicate must not cut the rollback short.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use grit_lib::progress::CancelToken;

static PROCESS_CANCEL: LazyLock<CancelToken> = LazyLock::new(CancelToken::new);

/// Monotonic time of the first interrupt, in nanoseconds.
static FIRST_INTERRUPT_NS: AtomicU64 = AtomicU64::new(0);

/// A further interrupt this soon after the first is the same interrupt delivered twice.
const DUPLICATE_WINDOW_NS: u64 = 100_000_000;

#[cfg(unix)]
const HANDLED_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Shared cancel handle wired to the signal handler; pass [`process_cancel_token`] into library calls.
pub fn process_cancel_token() -> CancelToken {
    PROCESS_CANCEL.clone()
}

/// Cancels the running library operation on the first interrupt; restores the previous handlers
/// when dropped.
pub struct InterruptCancel {
    #[cfg(unix)]
    previous: [libc::sigaction; 3],
}

/// Record an interrupt at monotonic time `now_ns` (only touches atomics, so it is
/// async-signal-safe).
///
/// Returns `true` when the interrupt is absorbed (the first one, or a duplicate of it) and
/// `false` when the process should terminate.
fn absorb_interrupt(now_ns: u64) -> bool {
    if !PROCESS_CANCEL.is_cancelled() {
        PROCESS_CANCEL.cancel();
        FIRST_INTERRUPT_NS.store(now_ns, Ordering::Relaxed);
        return true;
    }
    now_ns.saturating_sub(FIRST_INTERRUPT_NS.load(Ordering::Relaxed)) < DUPLICATE_WINDOW_NS
}

#[cfg(unix)]
fn monotonic_ns() -> u64 {
    // SAFETY: `clock_gettime` is async-signal-safe and only writes the provided struct.
    let ts = unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts
    };
    u64::try_from(ts.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(ts.tv_nsec).unwrap_or(0)
}

/// Console control handlers run on an ordinary thread, so `Instant` is fine here.
#[cfg(windows)]
fn monotonic_ns() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    u64::try_from(
        BASE.get_or_init(std::time::Instant::now)
            .elapsed()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(unix)]
extern "C" fn on_interrupt(signal: libc::c_int) {
    if absorb_interrupt(monotonic_ns()) {
        return;
    }
    // SAFETY: `signal` and `raise` are async-signal-safe. The signal stays blocked until this
    // handler returns, so the default action then terminates the process.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Returning `FALSE` passes the event on to the default handler, which ends the process.
/// Close, logoff and shutdown events are never absorbed.
#[cfg(windows)]
extern "system" fn on_console_ctrl(ctrl_type: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};

    let interrupt = ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT;
    windows_sys::core::BOOL::from(interrupt && absorb_interrupt(monotonic_ns()))
}

impl InterruptCancel {
    /// Route the next `SIGINT`, `SIGTERM`, or `SIGHUP` to [`CancelToken::is_cancelled`] instead of
    /// terminating.
    #[cfg(unix)]
    pub fn install() -> Self {
        process_cancel_token().reset();
        FIRST_INTERRUPT_NS.store(0, Ordering::Relaxed);
        // SAFETY: the handler only touches atomics and async-signal-safe libc calls, and
        // both `sigaction` structs are fully initialized before the call.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            for sig in HANDLED_SIGNALS {
                libc::sigaddset(&mut action.sa_mask, sig);
            }
            let mut previous = [std::mem::zeroed(); 3];
            for (sig, slot) in HANDLED_SIGNALS.iter().zip(previous.iter_mut()) {
                libc::sigaction(*sig, &action, slot);
            }
            Self { previous }
        }
    }

    /// Route the next console Ctrl+C or Ctrl+Break to [`CancelToken::is_cancelled`] instead of
    /// terminating.
    ///
    /// When the process has no console the registration still succeeds and simply never
    /// fires.
    #[cfg(windows)]
    pub fn install() -> Self {
        process_cancel_token().reset();
        FIRST_INTERRUPT_NS.store(0, Ordering::Relaxed);
        monotonic_ns();
        // SAFETY: registers a handler that only touches atomics; it is removed in `drop`.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_ctrl), 1);
        }
        Self {}
    }

    /// Interrupt signals keep their default meaning on platforms without handling here.
    #[cfg(not(any(unix, windows)))]
    pub fn install() -> Self {
        Self {}
    }
}

impl Drop for InterruptCancel {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: restores the actions saved by `install`.
        unsafe {
            for (sig, previous) in HANDLED_SIGNALS.iter().zip(self.previous.iter()) {
                libc::sigaction(*sig, previous, std::ptr::null_mut());
            }
        }
        #[cfg(windows)]
        // SAFETY: removes the handler registered by `install`.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_ctrl), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handler state is process-global; tests that install or raise must not overlap.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn first_interrupt_and_its_duplicate_are_absorbed_a_later_one_terminates() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        PROCESS_CANCEL.reset();
        let start = 5_000_000_000;
        assert!(absorb_interrupt(start), "the first interrupt cancels");
        assert!(PROCESS_CANCEL.is_cancelled());
        assert!(
            absorb_interrupt(start + DUPLICATE_WINDOW_NS - 1),
            "a duplicate delivery is absorbed"
        );
        assert!(
            !absorb_interrupt(start + DUPLICATE_WINDOW_NS),
            "a deliberate second Ctrl+C terminates"
        );
        PROCESS_CANCEL.reset();
    }

    #[cfg(unix)]
    fn current_handler(signal: libc::c_int) -> libc::sighandler_t {
        // SAFETY: querying the current action with a null new action.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, std::ptr::null(), &mut current);
            current.sa_sigaction
        }
    }

    #[test]
    fn process_cancel_token_shares_state_with_the_signal_handler() {
        #[cfg(unix)]
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        process_cancel_token().reset();
        let token = process_cancel_token();
        assert!(!token.is_cancelled());
        let _install = InterruptCancel::install();
        assert!(!process_cancel_token().is_cancelled());
        #[cfg(unix)]
        unsafe {
            libc::raise(libc::SIGINT);
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::CTRL_C_EVENT;
            assert_eq!(on_console_ctrl(CTRL_C_EVENT), 1);
        }
        assert!(token.is_cancelled());
        assert!(process_cancel_token().is_cancelled());
    }

    #[cfg(unix)]
    fn first_signal_cancels_a_duplicate_is_absorbed_and_drop_restores(signal: libc::c_int) {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = current_handler(signal);
        let cancel = InterruptCancel::install();
        let token = process_cancel_token();
        assert!(!token.is_cancelled());
        // SAFETY: the installed handler only sets atomic flags for these signals.
        unsafe {
            libc::raise(signal);
        }
        assert!(token.is_cancelled());
        // SAFETY: as above; a duplicate right after the first is absorbed.
        unsafe {
            libc::raise(signal);
        }
        assert!(token.is_cancelled());
        let ours = on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
        assert_eq!(
            current_handler(signal),
            ours,
            "the handler stays installed so a later signal can terminate"
        );
        drop(cancel);
        assert_eq!(current_handler(signal), before);
    }

    #[cfg(unix)]
    #[test]
    fn first_sigint_cancels_a_duplicate_is_absorbed_and_drop_restores_the_handler() {
        first_signal_cancels_a_duplicate_is_absorbed_and_drop_restores(libc::SIGINT);
    }

    #[cfg(unix)]
    #[test]
    fn first_sigterm_cancels_a_duplicate_is_absorbed_and_drop_restores_the_handler() {
        first_signal_cancels_a_duplicate_is_absorbed_and_drop_restores(libc::SIGTERM);
    }

    #[cfg(unix)]
    #[test]
    fn first_sighup_cancels_a_duplicate_is_absorbed_and_drop_restores_the_handler() {
        first_signal_cancels_a_duplicate_is_absorbed_and_drop_restores(libc::SIGHUP);
    }

    #[cfg(windows)]
    #[test]
    fn console_ctrl_c_and_ctrl_break_cancel_but_close_is_not_absorbed() {
        use windows_sys::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
        };

        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _cancel = InterruptCancel::install();
        let token = process_cancel_token();
        assert!(!token.is_cancelled());
        assert_eq!(on_console_ctrl(CTRL_CLOSE_EVENT), 0);
        assert!(!token.is_cancelled(), "closing the console is not a cancel");
        assert_eq!(on_console_ctrl(CTRL_C_EVENT), 1, "Ctrl+C is handled");
        assert!(token.is_cancelled());
        assert_eq!(
            on_console_ctrl(CTRL_BREAK_EVENT),
            1,
            "a duplicate is absorbed"
        );
        drop(_cancel);

        let _cancel = InterruptCancel::install();
        let token = process_cancel_token();
        assert!(!token.is_cancelled(), "install starts uncancelled");
        assert_eq!(
            on_console_ctrl(CTRL_BREAK_EVENT),
            1,
            "Ctrl+Break is handled"
        );
        assert!(token.is_cancelled());
    }
}
