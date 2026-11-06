//! Oracle helpers: compare grit rev-parse APIs to `git rev-parse --verify -q`.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::path::Path;

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    abbreviate_object_id, peel_to_tree, resolve_revision, resolve_revision_as_commit,
    resolve_revision_for_verify, resolve_treeish_blob_at_path, split_double_dot_range,
    split_triple_dot_range,
};
use grit_lib::revwalk::rev_parse_error::RevParseError;

use super::history_harness::git_rev_parse_verify;

/// How grit should resolve a spec for oracle comparison.
#[derive(Debug, Clone, Copy)]
pub enum ResolveMode {
    /// [`resolve_revision`]
    Revision,
    /// [`resolve_revision_for_verify`] (no index path DWIM)
    Verify,
    /// [`resolve_revision_as_commit`]
    AsCommit,
    /// Resolve then [`peel_to_tree`]
    PeelTree,
    /// [`resolve_treeish_blob_at_path`] (`rev:path`)
    TreeishBlobAtPath,
}

/// Outcome of comparing grit to git for one spec.
#[derive(Debug)]
pub enum OracleMismatch {
    GitOkGritErr {
        spec: String,
        git: ObjectId,
        grit_err: Error,
    },
    GitErrGritOk {
        spec: String,
        grit: ObjectId,
    },
    DifferentOid {
        spec: String,
        git: ObjectId,
        grit: ObjectId,
    },
    RevParseVariant {
        spec: String,
        expected: fn(&RevParseError) -> bool,
        got: Error,
    },
}

impl std::fmt::Display for OracleMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitOkGritErr {
                spec,
                git,
                grit_err,
            } => write!(f, "spec {spec:?}: git ok {git}, grit err {grit_err}"),
            Self::GitErrGritOk { spec, grit } => {
                write!(f, "spec {spec:?}: git failed, grit ok {grit}")
            }
            Self::DifferentOid { spec, git, grit } => {
                write!(f, "spec {spec:?}: git {git} != grit {grit}")
            }
            Self::RevParseVariant {
                spec,
                expected: _,
                got,
            } => write!(f, "spec {spec:?}: unexpected grit error {got}"),
        }
    }
}

fn grit_resolve(repo: &Repository, spec: &str, mode: ResolveMode) -> Result<ObjectId, Error> {
    match mode {
        ResolveMode::Revision => resolve_revision(repo, spec),
        ResolveMode::Verify => resolve_revision_for_verify(repo, spec),
        ResolveMode::AsCommit => resolve_revision_as_commit(repo, spec),
        ResolveMode::PeelTree => {
            let oid = resolve_revision_for_verify(repo, spec)?;
            peel_to_tree(repo, oid)
        }
        ResolveMode::TreeishBlobAtPath => Ok(resolve_treeish_blob_at_path(repo, spec)?.oid),
    }
}

/// Compare one spec against `git rev-parse --verify -q` for the same spelling.
pub fn oracle_one(
    repo: &Repository,
    worktree: &Path,
    spec: &str,
    mode: ResolveMode,
) -> Result<(), OracleMismatch> {
    let git = git_rev_parse_verify(worktree, spec);
    let grit = grit_resolve(repo, spec, mode);
    match (git, grit) {
        (Some(g), Ok(r)) if g == r => Ok(()),
        (Some(g), Ok(r)) => Err(OracleMismatch::DifferentOid {
            spec: spec.to_owned(),
            git: g,
            grit: r,
        }),
        (Some(g), Err(e)) => Err(OracleMismatch::GitOkGritErr {
            spec: spec.to_owned(),
            git: g,
            grit_err: e,
        }),
        (None, Ok(r)) => Err(OracleMismatch::GitErrGritOk {
            spec: spec.to_owned(),
            grit: r,
        }),
        (None, Err(_)) => Ok(()),
    }
}

/// When git fails, assert grit returns a specific [`RevParseError`] variant (not message text).
pub fn oracle_expect_rev_parse_error(
    repo: &Repository,
    worktree: &Path,
    spec: &str,
    mode: ResolveMode,
    pred: fn(&RevParseError) -> bool,
) -> Result<(), OracleMismatch> {
    if git_rev_parse_verify(worktree, spec).is_some() {
        let grit = grit_resolve(repo, spec, mode).expect("git failed but grit succeeded");
        return Err(OracleMismatch::GitErrGritOk {
            spec: spec.to_owned(),
            grit,
        });
    }
    match grit_resolve(repo, spec, mode) {
        Err(Error::RevParse(e)) if pred(&e) => Ok(()),
        Ok(oid) => Err(OracleMismatch::GitErrGritOk {
            spec: spec.to_owned(),
            grit: oid,
        }),
        Err(e) => Err(OracleMismatch::RevParseVariant {
            spec: spec.to_owned(),
            expected: pred,
            got: e,
        }),
    }
}

/// Run a table of specs; panics on first mismatch with a descriptive message.
pub fn oracle_table(repo: &Repository, worktree: &Path, mode: ResolveMode, specs: &[&str]) {
    for spec in specs {
        oracle_one(repo, worktree, spec, mode).unwrap_or_else(|e| panic!("{e}"));
    }
}

/// `git rev-parse --short` vs [`abbreviate_object_id`] for a full oid.
pub fn oracle_abbrev(repo: &Repository, worktree: &Path, oid: ObjectId, min_len: usize) {
    let grit = abbreviate_object_id(repo, oid, min_len).expect("grit abbrev");
    let git = super::history_harness::git(worktree, &["rev-parse", "--short", &oid.to_hex()]);
    let git = git.trim().to_string();
    assert_eq!(
        grit,
        git,
        "abbrev mismatch for {} (min_len {min_len})",
        oid.to_hex()
    );
}

/// Double-dot split oracle: grit split must match git's two rev-parse endpoints when both resolve.
pub fn oracle_double_dot_range(repo: &Repository, worktree: &Path, spec: &str) {
    let Some((left, right)) = split_double_dot_range(spec) else {
        assert!(
            git_rev_parse_verify(worktree, spec).is_none(),
            "git resolved undivided range spec {spec:?}"
        );
        return;
    };
    oracle_one(repo, worktree, left, ResolveMode::AsCommit)
        .unwrap_or_else(|e| panic!("left of {spec:?}: {e}"));
    oracle_one(repo, worktree, right, ResolveMode::AsCommit)
        .unwrap_or_else(|e| panic!("right of {spec:?}: {e}"));
}

/// Triple-dot split oracle.
pub fn oracle_triple_dot_range(repo: &Repository, worktree: &Path, spec: &str) {
    let Some((left, right)) = split_triple_dot_range(spec) else {
        return;
    };
    oracle_one(repo, worktree, left, ResolveMode::AsCommit)
        .unwrap_or_else(|e| panic!("left of {spec:?}: {e}"));
    oracle_one(repo, worktree, right, ResolveMode::AsCommit)
        .unwrap_or_else(|e| panic!("right of {spec:?}: {e}"));
}
