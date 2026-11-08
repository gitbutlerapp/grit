//! Optional Linux host hygiene for fair peak-RSS measurements.

use std::fs;
use std::io::Write;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Environment variable: when set, [`drop_page_caches_if_requested`] runs between daily scenarios.
pub const DROP_CACHES_ENV: &str = "GRIT_BENCH_DROP_CACHES";

const DROP_CACHES_PROC: &str = "/proc/sys/vm/drop_caches";
const MEMINFO: &str = "/proc/meminfo";

/// Drop the Linux page cache when [`DROP_CACHES_ENV`] is present in the environment.
///
/// Without `CAP_SYS_ADMIN`, writes to [`DROP_CACHES_PROC`] fail. When passwordless `sudo` is
/// available (`sudo -n`), this uses it. The operation is verified by reading `Cached:` from
/// [`MEMINFO`] before and after; if the cache does not shrink, returns an error instead of
/// continuing silently (sysctl can exit 0 while printing "permission denied").
pub fn drop_page_caches_if_requested() -> Result<()> {
    if std::env::var_os(DROP_CACHES_ENV).is_none() {
        return Ok(());
    }
    drop_linux_page_caches()
}

/// Always attempt to drop page, dentry, and inode caches (levels 1+2+3).
#[cfg(target_os = "linux")]
pub fn drop_linux_page_caches() -> Result<()> {
    let cached_before = meminfo_cached_kib().context("read Cached before drop_caches")?;
    run_sync()?;

    if write_drop_caches_direct().is_ok() {
        verify_drop(cached_before, "direct /proc/sys/vm/drop_caches write")?;
        return Ok(());
    }

    let sudo_status = Command::new("sudo")
        .args(["-n", "sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"])
        .status()
        .context("spawn sudo for drop_caches")?;
    if !sudo_status.success() {
        bail!(
            "GRIT_BENCH_DROP_CACHES is set but page caches could not be dropped \
             (need write access to {DROP_CACHES_PROC} or passwordless sudo); \
             direct write and sudo -n both failed"
        );
    }
    verify_drop(cached_before, "sudo -n drop_caches")?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn drop_linux_page_caches() -> Result<()> {
    if std::env::var_os(DROP_CACHES_ENV).is_some() {
        bail!("GRIT_BENCH_DROP_CACHES is only supported on Linux");
    }
    Ok(())
}

fn run_sync() -> Result<()> {
    let status = Command::new("sync").status().context("spawn sync")?;
    if !status.success() {
        bail!("sync failed with status {status}");
    }
    Ok(())
}

fn write_drop_caches_direct() -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(DROP_CACHES_PROC)
        .with_context(|| format!("open {DROP_CACHES_PROC}"))?;
    file.write_all(b"3")
        .with_context(|| format!("write {DROP_CACHES_PROC}"))?;
    Ok(())
}

fn verify_drop(cached_before_kib: u64, method: &str) -> Result<()> {
    let cached_after_kib = meminfo_cached_kib().context("read Cached after drop_caches")?;
    if cached_after_kib < cached_before_kib || cached_before_kib == 0 {
        return Ok(());
    }
    bail!(
        "drop_caches via {method} did not shrink the page cache \
         (Cached {cached_before_kib} KiB -> {cached_after_kib} KiB); \
         refusing to continue with GRIT_BENCH_DROP_CACHES"
    );
}

/// Parse the `Cached:` field from Linux `/proc/meminfo` (value in kibibytes).
pub fn meminfo_cached_kib() -> Result<u64> {
    let text = fs::read_to_string(MEMINFO).with_context(|| format!("read {MEMINFO}"))?;
    parse_meminfo_cached_kib(&text)
}

fn parse_meminfo_cached_kib(meminfo: &str) -> Result<u64> {
    for line in meminfo.lines() {
        let Some(rest) = line.strip_prefix("Cached:") else {
            continue;
        };
        let kib: u64 = rest
            .split_whitespace()
            .next()
            .context("Cached line missing value")?
            .parse()
            .with_context(|| format!("parse Cached value in line `{line}`"))?;
        return Ok(kib);
    }
    bail!("Cached field missing from {MEMINFO}");
}

/// Returns whether `sysctl -w vm.drop_caches=3` stderr indicates a permission denial that still
/// yields exit status 0 (the failure mode that made the old helper a silent no-op).
#[must_use]
pub fn sysctl_drop_caches_denied(stderr: &str) -> bool {
    stderr.contains("permission denied") && stderr.contains("vm.drop_caches")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cached_from_meminfo_sample() {
        let sample = "MemTotal:       16384000 kB\nCached:           877316 kB\n";
        assert_eq!(parse_meminfo_cached_kib(sample).unwrap(), 877_316);
    }

    #[test]
    fn sysctl_denial_pattern_matches_factory_vm() {
        let stderr = "sysctl: permission denied on key \"vm.drop_caches\", ignoring\n";
        assert!(sysctl_drop_caches_denied(stderr));
    }

    #[test]
    fn drop_caches_env_noop_when_unset() {
        std::env::remove_var(DROP_CACHES_ENV);
        drop_page_caches_if_requested().expect("unset env is a no-op");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn drop_caches_verified_when_env_set() {
        std::env::set_var(DROP_CACHES_ENV, "1");
        let result = drop_page_caches_if_requested();
        std::env::remove_var(DROP_CACHES_ENV);
        result.expect("factory VM should drop caches via sudo -n");
    }
}
