//! Cargo-shaped repository for repack benchmarks and memory budgets.
//!
//! Models what `grit clone` of a mid-size Rust project leaves behind: one pack
//! with deltas, a few thousand commits over a nested crate layout, hot files
//! (`Cargo.lock`, `CHANGELOG.md`) touched by many commits, files added over
//! time, and release tags. Built with `git fast-import` from a fixed seed, so
//! every machine gets the same objects.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Size of a cargo-shaped fixture.
#[derive(Clone, Copy, Debug)]
pub struct CargoShape {
    /// Commits on `main`.
    pub commits: usize,
    /// Files in the first commit (more are added along the way).
    pub files: usize,
}

impl CargoShape {
    /// Quick shape for tests in the gate (a pack of a few MiB).
    pub const SMOKE: Self = Self {
        commits: 2_000,
        files: 1_000,
    };
    /// Benchmark shape, about a third of rust-lang/cargo's history.
    pub const BENCH: Self = Self {
        commits: 8_000,
        files: 1_200,
    };
}

const DIRS: &[&str] = &[
    "src/cargo/core",
    "src/cargo/core/compiler",
    "src/cargo/core/resolver",
    "src/cargo/ops",
    "src/cargo/ops/cargo_compile",
    "src/cargo/sources/registry",
    "src/cargo/util",
    "src/cargo/util/toml",
    "src/bin/cargo/commands",
    "src/doc/src/reference",
    "tests/testsuite",
    "tests/testsuite/cargo_add",
    "crates/cargo-util/src",
    "crates/resolver-tests/src",
    "crates/xtask/src",
];

/// Deterministic xorshift generator; the fixture must not depend on the platform RNG.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

struct File {
    path: String,
    lines: Vec<String>,
}

/// One line of "source"; the random identifiers keep blobs about as
/// compressible as real code.
fn source_line(rng: &mut Rng, path: &str, n: usize) -> String {
    let ident = rng.next();
    match rng.below(4) {
        0 => format!("    let v{ident:x} = compute(\"{path}\", {n});"),
        1 => format!("    // keeps behaviour {ident:x} stable"),
        2 => format!("fn helper_{n}_{ident:x}() -> Result<()> {{ Ok(()) }}"),
        _ => format!("    assert_eq!(state.f{:x}, {n});", ident >> 20),
    }
}

fn new_file(rng: &mut Rng, path: String) -> File {
    let len = 40 + rng.below(260);
    let lines = (0..len).map(|n| source_line(rng, &path, n)).collect();
    File { path, lines }
}

/// Edit a few lines the way a commit to a source file does.
fn edit(rng: &mut Rng, file: &mut File, commit: usize) {
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(file.lines.len());
        match rng.below(5) {
            0 => file
                .lines
                .insert(at, format!("    // added in change {commit}")),
            1 if file.lines.len() > 20 => {
                file.lines.remove(at);
            }
            _ => file.lines[at] = source_line(rng, &file.path, commit),
        }
    }
}

fn write_blob(out: &mut String, path: &str, body: &str) {
    let _ = write!(out, "M 100644 inline {path}\ndata {}\n{body}\n", body.len());
}

/// `git fast-import` stream for `shape`.
#[must_use]
pub fn cargo_shape_fast_import(shape: CargoShape) -> Vec<u8> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut files: Vec<File> = (0..shape.files)
        .map(|i| {
            let dir = DIRS[i % DIRS.len()];
            new_file(&mut rng, format!("{dir}/module_{i}.rs"))
        })
        .collect();
    let mut lock: Vec<String> = (0..400)
        .map(|i| format!("name = \"dep-{i}\"\nversion = \"0.{}.0\"", i % 30))
        .collect();
    let mut changelog: Vec<String> = Vec::new();
    let mut out = String::new();
    for c in 0..shape.commits {
        let msg = format!("change {c}: update {}\n", files[c % files.len()].path);
        let _ = writeln!(out, "commit refs/heads/main\nmark :{}", c + 1);
        let _ = writeln!(
            out,
            "author Dev <dev@example.com> {} +0000",
            1_500_000_000 + c * 3_600
        );
        let _ = writeln!(
            out,
            "committer Dev <dev@example.com> {} +0000",
            1_500_000_000 + c * 3_600
        );
        let _ = write!(out, "data {}\n{msg}", msg.len());
        if c > 0 {
            let _ = writeln!(out, "from :{c}");
        }
        if c == 0 {
            for f in &files {
                write_blob(&mut out, &f.path, &f.lines.join("\n"));
            }
        }
        // Most commits touch the same few hundred files again.
        for _ in 0..1 + rng.below(3) {
            let pick = if rng.below(10) < 7 {
                rng.below(files.len().min(150))
            } else {
                rng.below(files.len())
            };
            edit(&mut rng, &mut files[pick], c);
            write_blob(&mut out, &files[pick].path, &files[pick].lines.join("\n"));
        }
        if c % 25 == 0 {
            let dir = DIRS[rng.below(DIRS.len())];
            let file = new_file(&mut rng, format!("{dir}/added_{c}.rs"));
            write_blob(&mut out, &file.path, &file.lines.join("\n"));
            files.push(file);
        }
        if c % 3 == 0 {
            let at = rng.below(lock.len());
            lock[at] = format!("name = \"dep-{at}\"\nversion = \"0.{}.{c}\"", at % 30);
            write_blob(&mut out, "Cargo.lock", &lock.join("\n"));
        }
        if c % 5 == 0 {
            changelog.insert(0, format!("- change {c}"));
            write_blob(&mut out, "CHANGELOG.md", &changelog.join("\n"));
        }
        if c % 500 == 499 {
            let _ = writeln!(out, "reset refs/tags/v0.{}.0\nfrom :{}", c / 500, c + 1);
        }
    }
    out.into_bytes()
}

fn git(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new(git)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Create a cargo-shaped repository with a work tree at `dest` (which must not
/// exist yet), packed into one pack with deltas like a fresh clone.
///
/// # Errors
///
/// Fails when `git` cannot be run or rejects the import.
pub fn build_cargo_shape_repo(git_bin: &Path, dest: &Path, shape: CargoShape) -> Result<()> {
    std::fs::create_dir_all(dest).context("create fixture dir")?;
    git(git_bin, dest, &["init", "-q", "-b", "main"])?;
    let mut child = Command::new(git_bin)
        .args(["fast-import", "--quiet"])
        .current_dir(dest)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git fast-import")?;
    child
        .stdin
        .take()
        .context("fast-import stdin")?
        .write_all(&cargo_shape_fast_import(shape))
        .context("feed fast-import")?;
    let out = child.wait_with_output().context("wait fast-import")?;
    if !out.status.success() {
        bail!(
            "git fast-import failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    git(git_bin, dest, &["reset", "-q", "--hard", "main"])?;
    git(git_bin, dest, &["repack", "-adfq"])?;
    git(git_bin, dest, &["prune-packed"])?;
    Ok(())
}

/// Build the `shape` fixture once under the bench scratch directory and return it.
///
/// # Errors
///
/// Fails when the fixture cannot be built (see [`build_cargo_shape_repo`]).
pub fn ensure_cargo_shape_repo(git_bin: &Path, shape: CargoShape) -> Result<std::path::PathBuf> {
    let dest = crate::fixture::scratch_dir()
        .join(format!("cargo-shape-{}c-{}f", shape.commits, shape.files));
    let ready = dest.with_extension("ready");
    if ready.is_file() && dest.join(".git").is_dir() {
        return Ok(dest);
    }
    if dest.exists() {
        crate::fixture::remove_dir_robust(&dest);
    }
    build_cargo_shape_repo(git_bin, &dest, shape)?;
    std::fs::write(&ready, b"ok").context("write cargo-shape ready marker")?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_import_stream_is_deterministic() {
        let shape = CargoShape {
            commits: 60,
            files: 20,
        };
        assert_eq!(
            cargo_shape_fast_import(shape),
            cargo_shape_fast_import(shape)
        );
    }
}
