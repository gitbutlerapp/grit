//! Deterministic chromium-sized synthetic repository generator (`git fast-import`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::bench_env::isolated_env_prefix;
use crate::fixture::remove_dir_robust;
use crate::real_repo_fixture::{git_env, run_git, scratch_root};

/// Parameters for the chromium-class synthetic benchmark repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromiumSynthSpec {
    pub file_count: usize,
    pub dir_count: usize,
    pub commit_count: usize,
    pub ref_count: usize,
    pub seed: u64,
}

impl ChromiumSynthSpec {
    /// Production chromium-class shape (cached under the bench scratch root).
    pub fn production() -> Self {
        Self {
            file_count: 400_000,
            dir_count: 40_000,
            commit_count: 100_000,
            ref_count: 50_000,
            seed: 0x4352_4947, // "CRIG" — stable campaign seed
        }
    }

    /// Small shape for unit tests and `--smoke` campaign runs.
    pub fn smoke() -> Self {
        Self {
            file_count: 200,
            dir_count: 20,
            commit_count: 50,
            ref_count: 10,
            seed: 42,
        }
    }

    /// Cache key fragment written into the ready marker.
    pub fn cache_key(&self) -> String {
        format!(
            "f{}-d{}-c{}-r{}-s{}",
            self.file_count, self.dir_count, self.commit_count, self.ref_count, self.seed
        )
    }
}

/// Simple deterministic PRNG (xorshift64*) for reproducible blob bytes.
#[derive(Debug, Clone, Copy)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// Create a generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        let state = seed.max(1);
        Self { state }
    }

    /// Return the next u64 and advance the state.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Fill `out` with deterministic bytes derived from the stream.
    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let n = self.next_u64();
            let bytes = n.to_le_bytes();
            for (dst, src) in chunk.iter_mut().zip(bytes.iter()) {
                *dst = *src;
            }
        }
    }
}

fn blob_body(rng: &mut DeterministicRng, index: usize) -> Vec<u8> {
    let len = 32 + (index % 64);
    let mut body = vec![0u8; len];
    rng.fill_bytes(&mut body);
    body
}

/// Write a full fast-import stream for `spec` into `out`.
pub fn write_fast_import_script(out: &mut Vec<u8>, spec: ChromiumSynthSpec) -> Result<()> {
    let mut rng = DeterministicRng::new(spec.seed);
    let files_per_dir = spec.file_count.div_ceil(spec.dir_count.max(1));

    for i in 0..spec.file_count {
        let body = blob_body(&mut rng, i);
        writeln!(out, "blob")?;
        writeln!(out, "mark :{}", i + 1)?;
        writeln!(out, "data {}", body.len())?;
        out.write_all(&body)?;
    }

    writeln!(out, "commit refs/heads/main")?;
    let initial_mark = spec.file_count + 1;
    writeln!(out, "mark :{initial_mark}")?;
    writeln!(out, "committer Bench <b@example.com> 1 +0000")?;
    let initial_msg = b"initial\n";
    writeln!(out, "data {}", initial_msg.len())?;
    out.write_all(initial_msg)?;

    for i in 0..spec.file_count {
        let dir = (i / files_per_dir).min(spec.dir_count.saturating_sub(1));
        let file = i % files_per_dir;
        let path = format!("d{dir:05}/f{file:05}.bin");
        writeln!(out, "M 100644 :{} {path}", i + 1)?;
    }

    let mut prev_mark = initial_mark;
    for c in 1..spec.commit_count {
        let ts = 1 + c as u64;
        let mark = spec.file_count + 1 + c;
        writeln!(out, "commit refs/heads/main")?;
        writeln!(out, "mark :{mark}")?;
        writeln!(out, "committer Bench <b@example.com> {ts} +0000")?;
        let msg = format!("msg {c}\n");
        writeln!(out, "data {}", msg.len())?;
        write!(out, "{msg}")?;
        writeln!(out, "from :{prev_mark}")?;
        prev_mark = mark;
    }

    for r in 0..spec.ref_count {
        let commit_index = r % spec.commit_count.max(1);
        let mark = spec.file_count + 1 + commit_index;
        writeln!(out, "reset refs/heads/bench-ref-{r:05}")?;
        writeln!(out, "from :{mark}")?;
    }
    Ok(())
}

fn run_git_fast_import(git: &Path, dest: &Path, payload: &[u8]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["fast-import", "--quiet"])
        .current_dir(dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    git_env(&mut cmd);
    let mut child = cmd.spawn().context("spawn git fast-import")?;
    {
        let stdin = child.stdin.as_mut().context("fast-import stdin")?;
        stdin.write_all(payload)?;
    }
    let out = child.wait_with_output().context("wait fast-import")?;
    if !out.status.success() {
        bail!(
            "git fast-import failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Path to the cached chromium-class bare repository for `spec`.
pub fn chromium_bare_cache_path(spec: ChromiumSynthSpec) -> PathBuf {
    scratch_root().join(format!("chromium-synth-{}.git", spec.cache_key()))
}

const CHROMIUM_READY: &str = ".grit-bench-chromium-ready";

/// Build or reuse a repacked bare chromium-class repository.
pub fn ensure_chromium_synthetic(git: &Path, spec: ChromiumSynthSpec) -> Result<PathBuf> {
    let dest = chromium_bare_cache_path(spec);
    let marker = dest.join(CHROMIUM_READY);
    if marker.is_file() {
        let stored = fs::read_to_string(&marker).context("read chromium ready marker")?;
        if stored.trim() == spec.cache_key() && dest.join("HEAD").is_file() {
            return Ok(dest);
        }
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let parent = dest.parent().unwrap_or(Path::new("/tmp"));
    std::fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("chromium bare name")?;
    run_git(git, parent, &["init", "--bare", "-q", "-b", "main", name])?;
    eprintln!(
        "Building chromium-class synthetic repo ({}) at {} …",
        spec.cache_key(),
        dest.display()
    );
    let mut script = Vec::new();
    write_fast_import_script(&mut script, spec)?;
    run_git_fast_import(git, &dest, &script)?;
    eprintln!("Repacking chromium synthetic (single pack) …");
    run_git(git, &dest, &["repack", "-a", "-d", "-f"])?;
    run_git(git, &dest, &["commit-graph", "write", "--reachable"])?;
    std::fs::write(&marker, spec.cache_key()).context("write chromium ready marker")?;
    Ok(dest)
}

/// Isolated git config prefix for campaign scenarios on real fixtures.
pub fn campaign_isolated_env() -> String {
    isolated_env_prefix()
}

use std::fs;

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn which_git() -> PathBuf {
        crate::binary::resolve_binary("git", None).expect("git")
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn deterministic_tree_oid_at_smoke_scale() -> Result<()> {
        let spec = ChromiumSynthSpec::smoke();
        let git = which_git();
        let a = TempDir::new()?;
        let b = TempDir::new()?;
        run_git(&git, a.path(), &["init", "-q", "-b", "main"])?;
        run_git(&git, b.path(), &["init", "-q", "-b", "main"])?;
        let mut script = Vec::new();
        write_fast_import_script(&mut script, spec)?;
        run_git_fast_import(&git, a.path(), &script)?;
        run_git_fast_import(&git, b.path(), &script)?;
        let tree_a = rev_parse_tree(&git, a.path())?;
        let tree_b = rev_parse_tree(&git, b.path())?;
        assert_eq!(tree_a, tree_b, "same seed must yield identical tree OIDs");
        Ok(())
    }

    fn rev_parse_tree(git: &Path, dir: &Path) -> Result<String> {
        let out = Command::new(git)
            .args(["rev-parse", "HEAD^{tree}"])
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()?;
        if !out.status.success() {
            bail!("rev-parse tree failed");
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}
