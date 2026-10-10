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
    use std::thread;

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn capped benchmark command")?;

    let pid = child.id();
    let start = Instant::now();
    let mut peak_rss_bytes = 0u64;
    let poll = Duration::from_millis(50);

    loop {
        if let Some(limit) = timeout {
            if start.elapsed() >= limit {
                kill_process_tree(pid);
                let _ = child.wait();
                return Ok(CappedRunOutcome {
                    termination: TerminationKind::Timeout,
                    peak_rss_bytes,
                });
            }
        }

        let rss = process_tree_rss_bytes(pid)?;
        peak_rss_bytes = peak_rss_bytes.max(rss);
        if let Some(cap) = max_rss_bytes {
            if rss > cap {
                kill_process_tree(pid);
                let _ = child.wait();
                return Ok(CappedRunOutcome {
                    termination: TerminationKind::MemoryCap {
                        peak_rss_bytes,
                        cap_bytes: cap,
                    },
                    peak_rss_bytes,
                });
            }
        }

        match child.try_wait().context("poll capped command")? {
            Some(status) => {
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
                });
            }
            None => thread::sleep(poll),
        }
    }
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
    })
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

#[cfg(unix)]
fn read_process_rss_bytes(pid: u32) -> Result<Option<u64>> {
    let path = format!("/proc/{pid}/status");
    let status = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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

    #[cfg(unix)]
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
}
