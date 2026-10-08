//! Deterministic repository fixtures for pack and fsck tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::git_check::object_format_env;
use super::hash::HashAlgo;

const AUTHOR_NAME: &str = "Pack Fixture Author";
const AUTHOR_EMAIL: &str = "pack-fixture@example.com";
const DETERMINISTIC_DATE: &str = "1700000000 +0000";

/// A temporary Git repository with isolated config and fixed identity metadata.
pub struct RepoFixture {
    root: tempfile::TempDir,
    algo: HashAlgo,
}

impl RepoFixture {
    /// Initialize a new repository using `algo` (`sha1` or `sha256`).
    ///
    /// Returns an error when `git init` fails (including unsupported SHA-256).
    pub fn init(algo: HashAlgo) -> std::io::Result<Self> {
        let root = tempfile::tempdir()?;
        let mut init_args = vec!["init", "-q"];
        if matches!(algo, HashAlgo::Sha256) {
            init_args.push("--object-format=sha256");
        }
        let status = Command::new("git")
            .current_dir(root.path())
            .args(&init_args)
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .env("GIT_OBJECT_FORMAT", object_format_env(algo))
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "git init failed for {:?}",
                algo
            )));
        }
        Ok(Self { root, algo })
    }

    /// Repository root (working tree for non-bare repos).
    #[must_use]
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Object hash algorithm selected at initialization.
    #[must_use]
    pub const fn algo(&self) -> HashAlgo {
        self.algo
    }

    /// Path to the `objects` directory (`<root>/.git/objects`).
    #[must_use]
    pub fn objects_dir(&self) -> PathBuf {
        self.path().join(".git").join("objects")
    }

    /// Run `git` in this repository with deterministic author/committer metadata.
    pub fn git(&self, args: &[&str]) -> GitRunOutcome {
        match Command::new("git")
            .current_dir(self.path())
            .args(args)
            .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
            .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
            .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
            .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
            .env("GIT_AUTHOR_DATE", DETERMINISTIC_DATE)
            .env("GIT_COMMITTER_DATE", DETERMINISTIC_DATE)
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .env("GIT_OBJECT_FORMAT", object_format_env(self.algo))
            .output()
        {
            Ok(out) => GitRunOutcome {
                ok: out.status.success(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => GitRunOutcome {
                ok: false,
                stdout: String::new(),
                stderr: format!("spawn git: {e}"),
            },
        }
    }
}

/// Captured output from [`RepoFixture::git`].
#[derive(Debug, Clone)]
pub struct GitRunOutcome {
    /// Whether the command succeeded.
    pub ok: bool,
    /// Standard output (UTF-8 lossy).
    pub stdout: String,
    /// Standard error (UTF-8 lossy).
    pub stderr: String,
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}
