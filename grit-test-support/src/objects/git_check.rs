//! Structured wrappers around system `git` object-store validators.

use std::path::Path;
use std::process::Command;

use super::hash::HashAlgo;

/// Outcome of `git fsck` with parsed camelCase message ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckOutcome {
    /// Whether `git fsck` exited with status zero.
    pub ok: bool,
    /// Documented fsck msg-ids appearing in stdout/stderr (for example `badTree`).
    pub msg_ids: Vec<String>,
}

/// Generic success/failure for auxiliary Git tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitToolOutcome {
    /// Whether the command exited successfully.
    pub ok: bool,
    /// Combined stdout and stderr (UTF-8 lossy) for diagnostics.
    pub output: String,
}

/// Result of `git cat-file --batch-check` over requested oids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchCheckOutcome {
    /// Whether every requested object was reported as present and well-formed.
    pub ok: bool,
    /// Raw batch-check lines (stdout, UTF-8 lossy).
    pub lines: Vec<String>,
}

/// Probe whether the installed `git` supports `--object-format=sha256`.
#[must_use]
pub fn git_supports_sha256() -> bool {
    let scratch = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    Command::new("git")
        .current_dir(scratch.path())
        .args(["init", "-q", "--object-format=sha256"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `git fsck` and collect documented msg-ids without panicking.
///
/// When `strict` is true, `--strict` is passed (matching Git's strict fsck mode).
pub fn git_fsck(repo: &Path, strict: bool) -> FsckOutcome {
    let mut args = vec!["fsck", "--no-dangling"];
    if strict {
        args.push("--strict");
    }
    let output = run_git(repo, &args);
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    FsckOutcome {
        ok: output.ok,
        msg_ids: parse_fsck_msg_ids(&combined),
    }
}

/// Run `git verify-pack -v` on `pack_path`.
pub fn git_verify_pack(repo: &Path, pack_path: &Path) -> GitToolOutcome {
    let rel = path_relative_to_repo(repo, pack_path);
    let output = run_git(repo, &["verify-pack", "-v", &rel.to_string_lossy()]);
    GitToolOutcome {
        ok: output.ok,
        output: format!("{}\n{}", output.stdout, output.stderr),
    }
}

/// Run `git commit-graph verify` when a commit-graph file exists.
pub fn git_commit_graph_verify(repo: &Path) -> GitToolOutcome {
    let output = run_git(repo, &["commit-graph", "verify"]);
    GitToolOutcome {
        ok: output.ok,
        output: format!("{}\n{}", output.stdout, output.stderr),
    }
}

/// Run `git midx verify` when a multi-pack-index file exists.
pub fn git_midx_verify(repo: &Path) -> GitToolOutcome {
    let output = run_git(repo, &["midx", "verify"]);
    GitToolOutcome {
        ok: output.ok,
        output: format!("{}\n{}", output.stdout, output.stderr),
    }
}

/// Run `git cat-file --batch-check` for each hex oid in `oids`.
pub fn git_cat_file_batch_check(repo: &Path, oids: &[&str]) -> BatchCheckOutcome {
    let mut child = match Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "--batch-check"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return BatchCheckOutcome {
                ok: false,
                lines: vec![format!("spawn failed: {e}")],
            };
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        for oid in oids {
            let _ = writeln!(stdin, "{oid}");
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return BatchCheckOutcome {
                ok: false,
                lines: vec![format!("wait failed: {e}")],
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = stdout.lines().map(str::to_owned).collect();
    let ok = output.status.success()
        && lines
            .iter()
            .all(|l| !l.contains(" missing") && !l.starts_with("missing"));

    BatchCheckOutcome { ok, lines }
}

struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run_git(repo: &Path, args: &[&str]) -> GitOutput {
    match Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
    {
        Ok(out) => GitOutput {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(e) => GitOutput {
            ok: false,
            stdout: String::new(),
            stderr: format!("spawn git: {e}"),
        },
    }
}

fn path_relative_to_repo(repo: &Path, path: &Path) -> std::path::PathBuf {
    path.strip_prefix(repo)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Parse camelCase fsck msg-ids from Git fsck output lines.
///
/// Only ids from Git's documented fsck message catalog are retained (conservative
/// filter: `[a-z]+[A-Z][A-Za-z0-9]*` immediately before a colon field).
fn parse_fsck_msg_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let rest = line
            .strip_prefix("error: ")
            .or_else(|| line.strip_prefix("warning: "))
            .or_else(|| line.strip_prefix("error in tree "))
            .or_else(|| line.strip_prefix("error in commit "))
            .or_else(|| line.strip_prefix("error in blob "))
            .or_else(|| line.strip_prefix("error in tag "));
        let Some(rest) = rest else {
            continue;
        };
        // Lines look like: `<object> badTree: detail` or `badSha1: ...`
        for token in rest.split_whitespace() {
            let Some(id) = token.strip_suffix(':') else {
                continue;
            };
            if is_documented_msg_id(id) && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
        // Also scan `: msgId:` segments deeper in the line.
        for segment in rest.split(':') {
            let id = segment.trim();
            if is_documented_msg_id(id) && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

fn is_documented_msg_id(s: &str) -> bool {
    if s.is_empty() || !s.as_bytes()[0].is_ascii_lowercase() {
        return false;
    }
    let bytes = s.as_bytes();
    let has_upper = bytes.iter().any(|b| b.is_ascii_uppercase());
    has_upper && bytes.iter().all(|b| b.is_ascii_alphanumeric())
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Configure `GIT_OBJECT_FORMAT` for a repository hash algorithm (used by fixtures).
pub(crate) fn object_format_env(algo: HashAlgo) -> &'static str {
    match algo {
        HashAlgo::Sha1 => "sha1",
        HashAlgo::Sha256 => "sha256",
    }
}
