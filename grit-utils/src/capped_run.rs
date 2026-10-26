//! Run shell commands with wall-clock timeout and peak-RSS monitoring (Unix).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Why a capped benchmark command stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationKind {
    /// Child exited successfully.
    Success,
    /// [`ServeCloneCaps`](crate::resource::ServeCloneCaps) wall-clock limit (GNU `timeout` uses 124).
    Timeout,
    /// RSS monitor killed the child before the host OOM killer ran ([`EXIT_MEMORY_CAP`]).
    MemoryCap {
        /// Peak RSS observed for the child (bytes).
        peak_rss_bytes: u64,
        /// Configured cap (bytes).
        cap_bytes: u64,
    },
    /// Child exited with a non-zero status (excluding timeout/memory cap codes).
    CommandFailed { code: i32 },
}

/// Exit code when the RSS monitor stops the child ([`TerminationKind::MemoryCap`]).
pub const EXIT_MEMORY_CAP: i32 = 125;

/// Exit code when the wall-clock timeout fires ([`TerminationKind::Timeout`]).
pub const EXIT_TIMEOUT: i32 = 124;

impl TerminationKind {
    /// Map a process exit code from [`run_capped_shell_command`] / `measure-rss`.
    #[must_use]
    pub fn from_exit_code(code: i32) -> Self {
        match code {
            0 => Self::Success,
            c if c == EXIT_TIMEOUT => Self::Timeout,
            c if c == EXIT_MEMORY_CAP => Self::MemoryCap {
                peak_rss_bytes: 0,
                cap_bytes: 0,
            },
            c => Self::CommandFailed { code: c },
        }
    }
}

/// Outcome of one monitored run.
#[derive(Debug, Clone)]
pub struct CappedRunOutcome {
    pub termination: TerminationKind,
    pub peak_rss_bytes: u64,
    /// Parent-observed wall time from spawn through exit or cap. On Linux it is read when the
    /// child exits; elsewhere it is rounded up to the next RSS poll tick.
    pub wall_ms: f64,
}

const RSS_POLL_INTERVAL: Duration = Duration::from_millis(1);

fn wall_ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Run `program` with `args` directly (no shell) under optional timeout and peak-RSS enforcement.
pub fn run_capped_argv_command(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Option<Duration>,
    max_rss_bytes: Option<u64>,
) -> Result<CappedRunOutcome> {
    run_capped_argv_command_with(program, args, cwd, timeout, max_rss_bytes, |_| {})
}

/// Like [`run_capped_argv_command`], but allows configuring the child `Command` before spawn.
#[cfg(unix)]
pub fn run_capped_argv_command_with(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Option<Duration>,
    max_rss_bytes: Option<u64>,
    configure: impl FnOnce(&mut std::process::Command),
) -> Result<CappedRunOutcome> {
    use std::process::{Command, Stdio};

    let mut child_cmd = Command::new(program);
    child_cmd
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure(&mut child_cmd);
    let child = child_cmd
        .spawn()
        .with_context(|| format!("spawn capped argv command {}", program.display()))?;

    monitor_child(child, Instant::now(), timeout, max_rss_bytes)
}

#[cfg(not(unix))]
pub fn run_capped_argv_command_with(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    _timeout: Option<Duration>,
    _max_rss_bytes: Option<u64>,
    configure: impl FnOnce(&mut std::process::Command),
) -> Result<CappedRunOutcome> {
    use std::process::{Command, Stdio};

    let mut child_cmd = Command::new(program);
    child_cmd
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure(&mut child_cmd);
    let status = child_cmd
        .status()
        .with_context(|| format!("run capped argv command {}", program.display()))?;
    let termination = if status.success() {
        TerminationKind::Success
    } else {
        TerminationKind::CommandFailed {
            code: status.code().unwrap_or(-1),
        }
    };
    Ok(CappedRunOutcome {
        termination,
        peak_rss_bytes: 0,
        wall_ms: 0.0,
    })
}

/// Run `command` under `/bin/sh -c` with optional timeout and peak-RSS enforcement.
///
/// When `max_rss_bytes` is `Some`, a parent polls `/proc/<pid>/status` and sends
/// `SIGKILL` once `VmRSS` exceeds the cap so the host OOM killer is not relied on.
#[cfg(unix)]
pub fn run_capped_shell_command(
    command: &str,
    cwd: &Path,
    timeout: Option<Duration>,
    max_rss_bytes: Option<u64>,
) -> Result<CappedRunOutcome> {
    use std::process::{Command, Stdio};

    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn capped benchmark command")?;

    monitor_child(child, Instant::now(), timeout, max_rss_bytes)
}

#[cfg(not(unix))]
pub fn run_capped_shell_command(
    command: &str,
    cwd: &Path,
    _timeout: Option<Duration>,
    _max_rss_bytes: Option<u64>,
) -> Result<CappedRunOutcome> {
    use std::process::{Command, Stdio};

    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run capped benchmark command")?;
    let termination = if status.success() {
        TerminationKind::Success
    } else {
        TerminationKind::CommandFailed {
            code: status.code().unwrap_or(-1),
        }
    };
    Ok(CappedRunOutcome {
        termination,
        peak_rss_bytes: 0,
        wall_ms: 0.0,
    })
}

/// Why the monitor stopped a child early.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
enum Stopped {
    Timeout,
    MemoryCap { cap_bytes: u64 },
}

/// Wait for `child` while enforcing `timeout` and `max_rss_bytes`, sampling tree RSS every
/// [`RSS_POLL_INTERVAL`].
///
/// On Linux the caller blocks in `waitid(WNOWAIT)` and reads the clock the moment the child
/// exits, while a watchdog thread samples and enforces the caps; the child is reaped only
/// after the watchdog stops, so it never signals a recycled pid. Timing on the poll loop
/// instead would round every run up to the next poll tick, which swamps millisecond commands.
#[cfg(target_os = "linux")]
fn monitor_child(
    mut child: std::process::Child,
    start: Instant,
    timeout: Option<Duration>,
    max_rss_bytes: Option<u64>,
) -> Result<CappedRunOutcome> {
    use nix::sys::wait::{waitid, Id, WaitPidFlag};
    use nix::unistd::Pid;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let pid = child.id();
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || -> (u64, Option<Stopped>) {
            let mut peak = 0u64;
            while !done.load(Ordering::Acquire) {
                if timeout.is_some_and(|limit| start.elapsed() >= limit) {
                    kill_process_tree(pid);
                    return (peak, Some(Stopped::Timeout));
                }
                let rss = process_tree_rss_bytes(pid).unwrap_or(0);
                peak = peak.max(rss);
                if let Some(cap_bytes) = max_rss_bytes.filter(|&cap| rss > cap) {
                    kill_process_tree(pid);
                    return (peak, Some(Stopped::MemoryCap { cap_bytes }));
                }
                std::thread::sleep(RSS_POLL_INTERVAL);
            }
            (peak, None)
        })
    };
    let exited = loop {
        match waitid(
            Id::Pid(Pid::from_raw(pid as i32)),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT,
        ) {
            Err(nix::errno::Errno::EINTR) => continue,
            other => break other,
        }
    };
    let wall_ms = wall_ms_since(start);
    done.store(true, Ordering::Release);
    let (sampled_peak, stopped) = watchdog
        .join()
        .map_err(|_| anyhow::anyhow!("RSS watchdog panicked"))?;
    exited.context("wait for capped command")?;
    let status = child.wait().context("reap capped command")?;
    let peak_rss_bytes = sampled_peak.max(children_rusage_peak_rss_bytes()?);
    let termination = match stopped {
        Some(Stopped::Timeout) => TerminationKind::Timeout,
        Some(Stopped::MemoryCap { cap_bytes }) => TerminationKind::MemoryCap {
            peak_rss_bytes,
            cap_bytes,
        },
        None if status.success() => TerminationKind::Success,
        None => TerminationKind::CommandFailed {
            code: status.code().unwrap_or(-1),
        },
    };
    Ok(CappedRunOutcome {
        termination,
        peak_rss_bytes,
        wall_ms,
    })
}

/// Polling fallback of the Linux monitor for Unix systems without `waitid(WNOWAIT)`.
#[cfg(all(unix, not(target_os = "linux")))]
fn monitor_child(
    mut child: std::process::Child,
    start: Instant,
    timeout: Option<Duration>,
    max_rss_bytes: Option<u64>,
) -> Result<CappedRunOutcome> {
    let pid = child.id();
    let mut peak_rss_bytes = 0u64;
    loop {
        let stopped = if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            Some(Stopped::Timeout)
        } else {
            let rss = process_tree_rss_bytes(pid)?;
            peak_rss_bytes = peak_rss_bytes.max(rss);
            max_rss_bytes
                .filter(|&cap| rss > cap)
                .map(|cap_bytes| Stopped::MemoryCap { cap_bytes })
        };
        if let Some(stopped) = stopped {
            kill_process_tree(pid);
            let _ = child.wait();
            peak_rss_bytes = peak_rss_bytes.max(children_rusage_peak_rss_bytes()?);
            let termination = match stopped {
                Stopped::Timeout => TerminationKind::Timeout,
                Stopped::MemoryCap { cap_bytes } => TerminationKind::MemoryCap {
                    peak_rss_bytes,
                    cap_bytes,
                },
            };
            return Ok(CappedRunOutcome {
                termination,
                peak_rss_bytes,
                wall_ms: wall_ms_since(start),
            });
        }
        if let Some(status) = child.try_wait().context("poll capped command")? {
            peak_rss_bytes = peak_rss_bytes.max(children_rusage_peak_rss_bytes()?);
            let termination = if status.success() {
                TerminationKind::Success
            } else {
                TerminationKind::CommandFailed {
                    code: status.code().unwrap_or(-1),
                }
            };
            return Ok(CappedRunOutcome {
                termination,
                peak_rss_bytes,
                wall_ms: wall_ms_since(start),
            });
        }
        std::thread::sleep(RSS_POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn kill_process_tree(root: u32) {
    if let Ok(pids) = collect_descendant_pids(root) {
        for pid in pids.into_iter().rev() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(root as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
}

#[cfg(unix)]
fn collect_descendant_pids(root: u32) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        out.push(pid);
        let path = format!("/proc/{pid}/task/{pid}/children");
        let children = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for token in children.split_whitespace() {
            if let Ok(child) = token.parse::<u32>() {
                stack.push(child);
            }
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn process_tree_rss_bytes(root: u32) -> Result<u64> {
    let pids = collect_descendant_pids(root)?;
    let mut total = 0u64;
    for pid in pids {
        if let Some(rss) = read_process_rss_bytes(pid)? {
            total = total.saturating_add(rss);
        }
    }
    Ok(total)
}

/// Peak RSS of all waited-for child processes ([`UsageWho::RUSAGE_CHILDREN`]).
#[cfg(unix)]
fn children_rusage_peak_rss_bytes() -> Result<u64> {
    use nix::sys::resource::{getrusage, UsageWho};
    let usage = getrusage(UsageWho::RUSAGE_CHILDREN).context("getrusage(RUSAGE_CHILDREN)")?;
    Ok(rss_bytes_from_rusage_max_rss(usage.max_rss()))
}

/// Convert `ru_maxrss` from `getrusage` into bytes (OS-specific units).
#[cfg(unix)]
pub(crate) fn rss_bytes_from_rusage_max_rss(raw: i64) -> u64 {
    let raw = raw.max(0) as u64;
    #[cfg(target_os = "macos")]
    {
        raw
    }
    #[cfg(not(target_os = "macos"))]
    {
        raw.saturating_mul(1024)
    }
}

#[cfg(unix)]
fn read_process_rss_bytes(pid: u32) -> Result<Option<u64>> {
    let path = format!("/proc/{pid}/status");
    let status = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if is_vanished_process_error(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .context("VmRSS field")?
                .parse()
                .context("parse VmRSS KiB")?;
            return Ok(Some(kb.saturating_mul(1024)));
        }
    }
    Ok(None)
}

/// Whether reading `/proc/<pid>/…` failed because the process exited meanwhile.
///
/// A process that exits after it was listed as a descendant either has no
/// `/proc` entry anymore (`NotFound`) or fails the read with `ESRCH` while it
/// is being torn down; both mean "nothing left to sample", not a harness error.
#[cfg(unix)]
fn is_vanished_process_error(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::NotFound
        || err.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termination_from_exit_code_maps_cap_and_timeout() {
        assert_eq!(TerminationKind::from_exit_code(0), TerminationKind::Success);
        assert_eq!(
            TerminationKind::from_exit_code(EXIT_TIMEOUT),
            TerminationKind::Timeout
        );
        assert_eq!(
            TerminationKind::from_exit_code(EXIT_MEMORY_CAP),
            TerminationKind::MemoryCap {
                peak_rss_bytes: 0,
                cap_bytes: 0
            }
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn rss_bytes_from_rusage_linux_kib_to_bytes() {
        if cfg!(target_os = "macos") {
            assert_eq!(rss_bytes_from_rusage_max_rss(4096), 4096);
        } else {
            assert_eq!(rss_bytes_from_rusage_max_rss(1024), 1024 * 1024);
        }
    }

    #[cfg(unix)]
    #[test]
    fn vanished_process_errors_are_not_harness_failures() {
        let esrch = std::io::Error::from_raw_os_error(nix::errno::Errno::ESRCH as i32);
        assert!(is_vanished_process_error(&esrch));
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(is_vanished_process_error(&missing));
        let denied = std::io::Error::from_raw_os_error(nix::errno::Errno::EACCES as i32);
        assert!(!is_vanished_process_error(&denied));
    }

    #[cfg(unix)]
    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exited_process_has_no_rss_sample() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait true");
        assert_eq!(read_process_rss_bytes(pid).expect("read rss"), None);
        assert_eq!(process_tree_rss_bytes(pid).expect("tree rss"), 0);
    }

    /// Prepare steps spawn many short-lived processes; sampling the tree while
    /// they exit must never fail the run.
    #[cfg(unix)]
    #[test]
    fn sampling_a_tree_of_exiting_processes_never_errors() {
        let mut shell = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("i=0; while [ $i -lt 4000 ]; do /bin/true; i=$((i+1)); done")
            .spawn()
            .expect("spawn shell loop");
        let pid = shell.id();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && shell.try_wait().expect("poll").is_none() {
            process_tree_rss_bytes(pid).expect("sample process tree");
        }
        let _ = shell.kill();
        let _ = shell.wait();
    }

    #[cfg(unix)]
    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn memory_monitor_kills_before_unbounded_growth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = "perl -e 'while(1){ $b .= \"x\" x (1024*1024) }'";
        let cap = 8 * 1024 * 1024;
        let outcome =
            run_capped_shell_command(script, dir.path(), Some(Duration::from_secs(15)), Some(cap))
                .expect("monitored run");
        assert!(
            matches!(
                outcome.termination,
                TerminationKind::MemoryCap { cap_bytes, .. } if cap_bytes == cap
            ),
            "unexpected termination: {:?} peak={}",
            outcome.termination,
            outcome.peak_rss_bytes
        );
        assert!(outcome.peak_rss_bytes >= cap);
    }

    #[cfg(unix)]
    #[test]
    fn wall_clock_timeout_uses_real_elapsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_capped_argv_command(
            Path::new("/bin/sleep"),
            &["2"],
            dir.path(),
            Some(Duration::from_secs(1)),
            None,
        )
        .expect("monitored run");
        assert_eq!(outcome.termination, TerminationKind::Timeout);
        assert!(
            outcome.wall_ms >= 900.0 && outcome.wall_ms < 1800.0,
            "expected ~1s wall at timeout, got {} ms",
            outcome.wall_ms
        );
    }

    /// The first poll after spawn almost never sees a fresh process exit, so polled timing
    /// reports at least one full poll interval even for a command that does nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn wall_time_is_read_at_exit_not_at_the_next_poll_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fastest = (0..30)
            .map(|_| {
                let outcome = run_capped_argv_command(
                    Path::new("/bin/true"),
                    &[],
                    dir.path(),
                    Some(Duration::from_secs(10)),
                    Some(1 << 30),
                )
                .expect("monitored run");
                assert_eq!(outcome.termination, TerminationKind::Success);
                outcome.wall_ms
            })
            .fold(f64::INFINITY, f64::min);
        assert!(
            fastest < RSS_POLL_INTERVAL.as_secs_f64() * 1000.0 * 0.9,
            "fastest /bin/true took {fastest} ms; timing still waits for a poll tick"
        );
    }

    #[cfg(unix)]
    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exit_status_and_peak_rss_survive_blocking_wait() {
        let dir = tempfile::tempdir().expect("tempdir");
        let failed = run_capped_shell_command("exit 3", dir.path(), None, None).expect("run");
        assert_eq!(
            failed.termination,
            TerminationKind::CommandFailed { code: 3 }
        );
        let grew = run_capped_shell_command(
            "perl -e '$b = \"x\" x (32*1024*1024); select(undef, undef, undef, 0.2)'",
            dir.path(),
            Some(Duration::from_secs(15)),
            Some(1 << 30),
        )
        .expect("run");
        assert_eq!(grew.termination, TerminationKind::Success);
        assert!(
            grew.peak_rss_bytes >= 32 * 1024 * 1024,
            "peak RSS {} misses the 32 MiB allocation",
            grew.peak_rss_bytes
        );
    }
}
