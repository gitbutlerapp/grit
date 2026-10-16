//! Thread-count environment for parallel scaling benchmarks.

use std::num::NonZeroUsize;

use crate::bench_env::isolated_env_prefix;
use crate::shell::shell_quote;

/// How worker threads are configured for a benchmark pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadMode {
    /// `index.threads`, `checkout.workers`, and `pack.threads` set to `1`.
    One,
    /// Unset thread keys so Git and grit use their default (all logical CPUs).
    Auto,
}

impl ThreadMode {
    /// Short suffix for scenario ids (`t1`, `tauto`).
    #[must_use]
    pub fn id_suffix(self) -> &'static str {
        match self {
            Self::One => "t1",
            Self::Auto => "tauto",
        }
    }

    /// Human-readable column label for docs tables.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::One => "1",
            Self::Auto => "auto",
        }
    }
}

/// Extra `grit` argv tokens after global isolation flags (for example `--threads`).
#[must_use]
pub fn grit_thread_argv(mode: ThreadMode, grit_supports_threads_flag: bool) -> Vec<String> {
    if !grit_supports_threads_flag {
        return Vec::new();
    }
    match mode {
        ThreadMode::One => vec!["--threads=1".into()],
        ThreadMode::Auto => vec!["--threads=0".into()],
    }
}

/// Whether the built `grit` binary advertises a global `--threads` flag.
pub fn grit_supports_threads_flag(grit: &std::path::Path) -> bool {
    std::process::Command::new(grit)
        .arg("--help")
        .output()
        .ok()
        .is_some_and(|o| {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            text.contains("--threads")
        })
}

/// Build a shell `env` prefix: hermetic git identity plus optional thread config keys.
#[must_use]
pub fn parallel_env_prefix(mode: ThreadMode) -> String {
    let mut parts: Vec<String> = isolated_env_prefix()
        .split_whitespace()
        .map(str::to_string)
        .collect();

    let (count, pairs) = thread_config_pairs(mode);
    parts.push(format!("GIT_CONFIG_COUNT={count}"));
    for (i, (key, value)) in pairs.iter().enumerate() {
        parts.push(format!("GIT_CONFIG_KEY_{i}={}", shell_quote(key)));
        parts.push(format!("GIT_CONFIG_VALUE_{i}={}", shell_quote(value)));
    }
    parts.join(" ")
}

fn thread_config_pairs(mode: ThreadMode) -> (usize, Vec<(&'static str, &'static str)>) {
    match mode {
        ThreadMode::One => (
            5,
            vec![
                ("gc.auto", "0"),
                ("maintenance.auto", "false"),
                ("index.threads", "1"),
                ("checkout.workers", "1"),
                ("pack.threads", "1"),
            ],
        ),
        ThreadMode::Auto => (2, vec![("gc.auto", "0"), ("maintenance.auto", "false")]),
    }
}

/// Resolved logical CPU count (for docs / metadata).
#[must_use]
pub fn logical_cpu_count() -> u32 {
    std::thread::available_parallelism()
        .ok()
        .and_then(|n| u32::try_from(n.get()).ok())
        .unwrap_or(1)
}

/// Thread count grit/git should use in `auto` mode on this host.
#[must_use]
pub fn auto_thread_count() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_thread_mode_sets_all_keys() {
        let prefix = parallel_env_prefix(ThreadMode::One);
        assert!(prefix.contains("index.threads"));
        assert!(prefix.contains("checkout.workers"));
        assert!(prefix.contains("pack.threads"));
        assert!(prefix.contains("GIT_CONFIG_COUNT=5"));
    }

    #[test]
    fn auto_thread_mode_omits_thread_keys() {
        let prefix = parallel_env_prefix(ThreadMode::Auto);
        assert!(!prefix.contains("index.threads"));
        assert!(prefix.contains("GIT_CONFIG_COUNT=2"));
    }
}
