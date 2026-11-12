//! Feed hook stdin without delivering `SIGPIPE` to the process.
//!
//! Embedding processes (for example `grit serve` with default `SIGPIPE` handling) must
//! survive a hook that exits before reading its stdin. How a write to a reader-less
//! pipe raises `SIGPIPE` differs by kernel, so two mechanisms combine:
//!
//! - Linux directs the signal at the writing thread. [`write_all`] and [`copy`] block
//!   `SIGPIPE` on that thread for the duration of the write and consume any signal
//!   the write queued, so the write fails with [`std::io::ErrorKind::BrokenPipe`].
//! - Apple kernels raise a *process-directed* signal, delivered to any thread that has
//!   not blocked it, so a per-thread mask cannot protect the process.
//!   [`disable_on_pipe`] sets `F_SETNOSIGPIPE` on the write end instead; the
//!   [`crate::command_runner`] spawn path applies it to every child stdin pipe.
//!
//! Callers ignore the resulting `BrokenPipe` when the hook already exited successfully.

use std::io::{self, Read, Write};

/// Copy all bytes from `reader` to `writer`, ignoring `SIGPIPE` on Unix.
///
/// On Apple targets `writer` must be a pipe prepared with [`disable_on_pipe`] for
/// the guarantee to hold (see the module docs).
pub(crate) fn copy(reader: &mut impl Read, writer: &mut (impl Write + ?Sized)) -> io::Result<u64> {
    without_sigpipe(|| io::copy(reader, writer))
}

/// Write `data` to `writer`, ignoring `SIGPIPE` on Unix.
///
/// On Apple targets `writer` must be a pipe prepared with [`disable_on_pipe`] for
/// the guarantee to hold (see the module docs).
pub(crate) fn write_all(writer: &mut (impl Write + ?Sized), data: &[u8]) -> io::Result<()> {
    without_sigpipe(|| writer.write_all(data))
}

/// Make writes to `pipe` fail with `EPIPE` instead of raising `SIGPIPE`.
///
/// Uses `F_SETNOSIGPIPE` on Apple targets, where `SIGPIPE` from a pipe write is
/// process-directed. Elsewhere this is a no-op: Linux has no per-descriptor switch for
/// pipes and directs the signal at the writing thread, which [`write_all`] handles.
///
/// # Parameters
///
/// - `pipe`: write end of a pipe (for example a child's [`std::process::ChildStdin`]).
///
/// # Errors
///
/// Returns the OS error when `fcntl` rejects the descriptor.
#[cfg(target_vendor = "apple")]
pub(crate) fn disable_on_pipe(pipe: &impl std::os::fd::AsFd) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    /// `F_SETNOSIGPIPE` from Apple's `<sys/fcntl.h>`; the `libc` crate does not export it.
    const F_SETNOSIGPIPE: libc::c_int = 73;

    // SAFETY: `fcntl` on a borrowed, open descriptor with an integer argument.
    let rc = unsafe { libc::fcntl(pipe.as_fd().as_raw_fd(), F_SETNOSIGPIPE, 1) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Make writes to `pipe` fail with `EPIPE` instead of raising `SIGPIPE`.
///
/// No-op on this target; see the Apple variant and the module docs.
///
/// # Errors
///
/// Never fails on this target.
#[cfg(all(unix, not(target_vendor = "apple")))]
pub(crate) fn disable_on_pipe(_pipe: &impl std::os::fd::AsFd) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn pthread_error(rc: i32) -> io::Error {
    io::Error::from_raw_os_error(rc)
}

#[cfg(unix)]
fn sigpipe_pending() -> io::Result<bool> {
    use std::mem::MaybeUninit;
    let mut pending = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigpending` fills `pending`.
    let rc = unsafe { libc::sigpending(pending.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { libc::sigismember(pending.as_ptr(), libc::SIGPIPE) != 0 })
}

#[cfg(unix)]
fn without_sigpipe<R, F: FnOnce() -> io::Result<R>>(write: F) -> io::Result<R> {
    use std::mem::MaybeUninit;
    use std::ptr;

    let pending_before = sigpipe_pending()?;

    let mut block = MaybeUninit::<libc::sigset_t>::uninit();
    let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: empty set, then add only SIGPIPE.
    unsafe {
        libc::sigemptyset(block.as_mut_ptr());
        libc::sigaddset(block.as_mut_ptr(), libc::SIGPIPE);
    }
    // SAFETY: `pthread_sigmask` fills `previous` on success.
    let mask_rc =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, block.as_ptr(), previous.as_mut_ptr()) };
    if mask_rc != 0 {
        return Err(pthread_error(mask_rc));
    }

    let result = write();

    // SAFETY: consume only a SIGPIPE this write newly queued while blocked, then restore
    // the caller's mask without delivering a stale pending signal.
    unsafe {
        if let Ok(pending_after) = sigpipe_pending() {
            if pending_after && !pending_before {
                let mut sig = 0i32;
                let _ = libc::sigwait(block.as_ptr(), &mut sig);
            }
        }
        let restore_rc =
            libc::pthread_sigmask(libc::SIG_SETMASK, previous.as_ptr(), ptr::null_mut());
        if restore_rc != 0 && result.is_ok() {
            return Err(pthread_error(restore_rc));
        }
    }

    result
}

#[cfg(not(unix))]
fn without_sigpipe<R, F: FnOnce() -> io::Result<R>>(write: F) -> io::Result<R> {
    write()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// Enough ref lines that hook stdin exceeds a typical 64 KiB pipe buffer once
    /// the hook has exited without reading.
    const REF_LINES: usize = 3000;

    /// Disposition to restore when the test holding the lock finishes.
    ///
    /// Tests that change the process-wide `SIGPIPE` disposition hold this lock so
    /// their save/restore pairs cannot interleave and leave `SIG_DFL` installed.
    #[cfg(unix)]
    static SIGPIPE_DISPOSITION: Mutex<Option<libc::sighandler_t>> = Mutex::new(None);

    /// Installs a `SIGPIPE` disposition and restores the previous one on drop.
    #[cfg(unix)]
    struct SigpipeDisposition {
        saved: MutexGuard<'static, Option<libc::sighandler_t>>,
    }

    #[cfg(unix)]
    impl SigpipeDisposition {
        fn install(handler: libc::sighandler_t) -> Self {
            let mut saved = SIGPIPE_DISPOSITION
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // SAFETY: `handler` is `SIG_DFL` or an async-signal-safe `extern "C"` fn.
            let previous = unsafe { libc::signal(libc::SIGPIPE, handler) };
            assert_ne!(previous, libc::SIG_ERR, "install SIGPIPE disposition");
            *saved = Some(previous);
            Self { saved }
        }
    }

    #[cfg(unix)]
    impl Drop for SigpipeDisposition {
        fn drop(&mut self) {
            if let Some(previous) = self.saved.take() {
                // SAFETY: restores the disposition saved by `install`.
                unsafe {
                    libc::signal(libc::SIGPIPE, previous);
                }
            }
        }
    }

    #[cfg(unix)]
    extern "C" fn ignore_signal(_sig: libc::c_int) {}

    fn hook_stdin_payload() -> Vec<u8> {
        (0..REF_LINES)
            .flat_map(|i| {
                format!(
                    "0000000000000000000000000000000000000000 \
                     deadbeefdeadbeefdeadbeefdeadbeefdeadbeef \
                     refs/heads/branch-{i}\n"
                )
                .into_bytes()
            })
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn without_sigpipe_preserves_preexisting_pending_sigpipe() {
        use std::mem::MaybeUninit;

        // The test harness ignores SIGPIPE, and Apple kernels discard an ignored signal
        // at generation even while it is blocked, so it could never become pending.
        // A no-op handler keeps it pending on every platform without risking SIG_DFL.
        let handler: extern "C" fn(libc::c_int) = ignore_signal;
        let _disposition = SigpipeDisposition::install(handler as libc::sighandler_t);

        let mut block = MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
        unsafe {
            libc::sigemptyset(block.as_mut_ptr());
            libc::sigaddset(block.as_mut_ptr(), libc::SIGPIPE);
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, block.as_ptr(), previous.as_mut_ptr());
            assert_eq!(rc, 0, "block SIGPIPE");
            libc::raise(libc::SIGPIPE);
        }
        assert!(
            sigpending_or_panic(),
            "expected SIGPIPE pending before without_sigpipe"
        );

        without_sigpipe(|| Ok::<(), io::Error>(())).expect("noop without_sigpipe");

        assert!(
            sigpending_or_panic(),
            "without_sigpipe must not consume caller-owned pending SIGPIPE"
        );

        unsafe {
            let mut sig = 0i32;
            let rc = libc::sigwait(block.as_ptr(), &mut sig);
            assert_eq!(rc, 0);
            assert_eq!(sig, libc::SIGPIPE);
            let restore_rc =
                libc::pthread_sigmask(libc::SIG_SETMASK, previous.as_ptr(), std::ptr::null_mut());
            assert_eq!(restore_rc, 0);
        }
    }

    #[cfg(unix)]
    fn sigpending_or_panic() -> bool {
        sigpipe_pending().expect("sigpending")
    }

    #[test]
    #[cfg(unix)]
    fn write_all_survives_sig_dfl_when_hook_ignores_stdin() {
        use crate::command_runner::RunningCommand;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};

        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let hook = std::env::temp_dir().join(format!("grit-sigpipe-hook-{n}"));
        std::fs::write(&hook, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _disposition = SigpipeDisposition::install(libc::SIG_DFL);
        let payload = Arc::new(hook_stdin_payload());
        let payload_thread = Arc::clone(&payload);
        let writer = std::thread::spawn(move || {
            let mut child = Command::new(&hook)
                .stdin(Stdio::piped())
                .spawn()
                .expect("spawn hook");
            let stdin = child.stdin.take();
            // Same wrapping as the hook runner's spawn path.
            let mut running = RunningCommand::from_system(child, stdin, None, None, false);
            let stdin = running.stdin_mut().expect("stdin");
            let write_result = write_all(stdin, &payload_thread);
            if let Err(err) = write_result {
                assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
            }
            running.close_stdin();
            let (mut child, ..) = running.into_system_parts().expect("system child");
            let status = child.wait().expect("wait");
            assert!(status.success());
        });
        writer.join().expect("writer thread survived SIGPIPE");
    }

    #[test]
    #[cfg(unix)]
    fn disable_on_pipe_accepts_child_stdin() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let stdin = child.stdin.take().expect("stdin");
        disable_on_pipe(&stdin).expect("disable SIGPIPE on pipe");
        drop(stdin);
        assert!(child.wait().expect("wait").success());
    }
}
