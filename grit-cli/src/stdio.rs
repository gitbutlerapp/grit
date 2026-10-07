//! Stdout behavior when the reader closes early (e.g. `grit log | head`).
//!
//! Rust ignores `SIGPIPE` by default, so writes to a closed pipe return
//! `std::io::ErrorKind::BrokenPipe` and `println!` panics. Git restores the default
//! handler so the process terminates with signal 13 (exit 141) instead of
//! printing a backtrace.

#[cfg(unix)]
pub fn configure() {
    // SAFETY: `signal(SIGPIPE, SIG_DFL)` is async-signal-safe on POSIX.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
pub fn configure() {}

/// Treat a closed stdout pipe as success (used for `--json` and flush paths).
pub(crate) fn io_result(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e),
    }
}
