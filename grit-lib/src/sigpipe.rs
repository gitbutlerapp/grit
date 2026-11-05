//! Feed hook stdin without delivering `SIGPIPE` to the writing thread.
//!
//! Embedding processes (for example `grit serve` with default `SIGPIPE` handling) must
//! survive a hook that exits before reading its stdin. Blocking `SIGPIPE` around the
//! write turns a closed pipe into [`std::io::ErrorKind::BrokenPipe`], which callers
//! ignore when the hook already exited successfully.

use std::io::{self, Read, Write};

/// Copy all bytes from `reader` to `writer`, ignoring `SIGPIPE` on Unix.
pub(crate) fn copy(reader: &mut impl Read, writer: &mut (impl Write + ?Sized)) -> io::Result<u64> {
    without_sigpipe(|| io::copy(reader, writer))
}

/// Write `data` to `writer`, ignoring `SIGPIPE` on Unix.
pub(crate) fn write_all(writer: &mut (impl Write + ?Sized), data: &[u8]) -> io::Result<()> {
    without_sigpipe(|| writer.write_all(data))
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
        return Err(io::Error::from_raw_os_error(rc));
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
    use std::sync::Arc;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// Enough ref lines that hook stdin exceeds a typical 64 KiB pipe buffer once
    /// the hook has exited without reading.
    const REF_LINES: usize = 3000;

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
            sigpipe_pending().expect("sigpending"),
            "expected SIGPIPE pending before without_sigpipe"
        );

        without_sigpipe(|| Ok::<(), io::Error>(())).expect("noop without_sigpipe");

        assert!(
            sigpipe_pending().expect("sigpending after"),
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

    #[test]
    #[cfg(unix)]
    fn write_all_survives_sig_dfl_when_hook_ignores_stdin() {
        use std::process::{Command, Stdio};

        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let hook = std::env::temp_dir().join(format!("grit-sigpipe-hook-{n}"));
        std::fs::write(&hook, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let payload = Arc::new(hook_stdin_payload());
        let payload_thread = Arc::clone(&payload);
        let writer = std::thread::spawn(move || {
            let mut child = Command::new(&hook)
                .stdin(Stdio::piped())
                .spawn()
                .expect("spawn hook");
            let mut stdin = child.stdin.take().expect("stdin");
            let write_result = write_all(&mut stdin, &payload_thread);
            if let Err(err) = write_result {
                assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
            }
            drop(stdin);
            let status = child.wait().expect("wait");
            assert!(status.success());
        });
        writer.join().expect("writer thread survived SIGPIPE");
        // SAFETY: restore previous disposition for other tests in this process.
        unsafe {
            libc::signal(libc::SIGPIPE, previous);
        }
    }
}
