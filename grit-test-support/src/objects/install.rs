//! Install pack bytes through system `git index-pack`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::hash::HashAlgo;

/// Index file format requested from `git index-pack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexVersion {
    /// Classic v1 index (32-bit offsets only).
    V1,
    /// Version 2 index; `large_offset_at` sets the 64-bit offset threshold (Git's
    /// `--index-version=2,<offset>`).
    V2LargeOffsetAt(u32),
}

/// Options forwarded to `git index-pack` when materializing `.idx` (and optional `.rev`).
#[derive(Debug, Clone, Default)]
pub struct IndexPackOptions {
    /// Pass `--rev-index` to build a reverse index alongside the pack index.
    pub rev_index: bool,
    /// When set, passes `--index-version=…` to force v1 or v2 large-offset layout.
    pub index_version: Option<IndexVersion>,
}

/// Result of writing and indexing a pack under a repository object directory.
#[derive(Debug, Clone)]
pub struct PackInstallOutcome {
    /// Path to the written `.pack` file.
    pub pack_path: PathBuf,
    /// Path to the generated `.idx` file when indexing succeeded.
    pub idx_path: Option<PathBuf>,
    /// Path to the generated `.rev` file when [`IndexPackOptions::rev_index`] was set.
    pub rev_path: Option<PathBuf>,
    /// Whether `git index-pack` exited successfully.
    pub index_ok: bool,
    /// Decoded stderr from `git index-pack` (UTF-8 lossy).
    pub index_stderr: String,
}

/// Write `pack` to `objects_dir/pack/{stem}.pack` and run `git index-pack`.
///
/// `objects_dir` is typically `<repo>/.git/objects` or `<bare-repo>/objects`.
/// `algo` selects the `--object-format` hint when the repository uses SHA-256.
pub fn write_pack_and_index(
    objects_dir: &Path,
    stem: &str,
    pack: &[u8],
    algo: HashAlgo,
    options: &IndexPackOptions,
) -> PackInstallOutcome {
    let pack_dir = objects_dir.join("pack");
    let _ = std::fs::create_dir_all(&pack_dir);
    let pack_path = pack_dir.join(format!("{stem}.pack"));
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    let rev_path = pack_dir.join(format!("{stem}.rev"));

    let write_err = std::fs::write(&pack_path, pack).err();
    if write_err.is_some() {
        return PackInstallOutcome {
            pack_path,
            idx_path: None,
            rev_path: None,
            index_ok: false,
            index_stderr: "failed to write pack file".into(),
        };
    }

    let mut args = vec!["index-pack".to_string(), "-v".to_string()];
    if options.rev_index {
        args.push("--rev-index".to_string());
    }
    if let Some(version) = options.index_version {
        match version {
            IndexVersion::V1 => args.push("--index-version=1".to_string()),
            IndexVersion::V2LargeOffsetAt(threshold) => {
                args.push(format!("--index-version=2,{threshold}"));
            }
        }
    }
    args.push(pack_path.to_string_lossy().into_owned());

    let repo_root = repo_root_from_objects(objects_dir);

    let out = Command::new("git")
        .current_dir(repo_root)
        .args(&args)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env(
            "GIT_OBJECT_FORMAT",
            if matches!(algo, HashAlgo::Sha256) {
                "sha256"
            } else {
                "sha1"
            },
        )
        .output();

    match out {
        Ok(output) => PackInstallOutcome {
            pack_path,
            idx_path: output.status.success().then_some(idx_path),
            rev_path: options
                .rev_index
                .then_some(rev_path)
                .filter(|_| output.status.success()),
            index_ok: output.status.success(),
            index_stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(e) => PackInstallOutcome {
            pack_path,
            idx_path: None,
            rev_path: None,
            index_ok: false,
            index_stderr: format!("spawn git index-pack: {e}"),
        },
    }
}

fn repo_root_from_objects(objects_dir: &Path) -> &Path {
    let Some(parent) = objects_dir.parent() else {
        return objects_dir;
    };
    if parent.file_name().is_some_and(|n| n == ".git") {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    }
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}
