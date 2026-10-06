//! Revision resolution helpers for porcelain commands that must match Git's error shapes.
//!
//! Some Git commands resolve commitishes **without** index path DWIM (`README` is not
//! `HEAD:README`). Filter options use `error: malformed object name …` and exit **129**; some
//! listing modes use `fatal: malformed object name …` and exit **128** (handled via
//! [`grit_lib::error::Error::Message`]).

use anyhow::Result;
use grit_lib::error::Error as LibError;
use grit_lib::objects::ObjectId;
use grit_lib::objects::{tag_object_line_oid, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision_without_index_dwim;

use crate::explicit_exit::ExplicitExit;

fn malformed_object_name_plain(spec: &str) -> String {
    spec.to_owned()
}

fn malformed_object_name_quoted(spec: &str) -> String {
    format!("'{spec}'")
}

/// Print Git-compatible lines for a non-commit object and exit 129 (`branch` / `tag --contains`).
pub fn exit_wrong_type_not_commit(hex: &str, kind: ObjectKind) -> Result<ObjectId> {
    let kind_msg = match kind {
        ObjectKind::Tree => "tree",
        ObjectKind::Blob => "blob",
        ObjectKind::Tag => "tag",
        ObjectKind::Commit => "commit",
    };
    Err(anyhow::Error::new(ExplicitExit {
        code: 129,
        message: format!(
            "error: object {hex} is a {kind_msg}, not a commit\nerror: no such commit {hex}"
        ),
    }))
}

/// Resolve `spec` for `tag --contains` / `--no-contains`, `branch --contains` / `--no-contains`,
/// and `for-each-ref` contains filters (plain `error: malformed object name`, exit 129).
pub fn resolve_porcelain_commitish_filter(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision_without_index_dwim(repo, spec).map_err(|_| {
        anyhow::Error::new(ExplicitExit {
            code: 129,
            message: format!(
                "error: malformed object name {}",
                malformed_object_name_plain(spec)
            ),
        })
    })?;

    if spec.len() == 40
        && spec.chars().all(|c| c.is_ascii_hexdigit())
        && repo.odb.read(&oid).is_err()
    {
        let hex = oid.to_hex();
        return Err(anyhow::Error::new(ExplicitExit {
            code: 129,
            message: format!("error: no such commit {hex}"),
        }));
    }

    let object = match repo.odb.read(&oid) {
        Ok(o) => o,
        Err(_) => {
            let hex = oid.to_hex();
            return Err(anyhow::Error::new(ExplicitExit {
                code: 129,
                message: format!("error: no such commit {hex}"),
            }));
        }
    };

    match object.kind {
        ObjectKind::Commit => Ok(oid),
        _ => exit_wrong_type_not_commit(&oid.to_hex(), object.kind),
    }
}

/// Resolve `spec` for `tag --points-at` and `for-each-ref --points-at` (quoted malformed names,
/// exit 129).
///
/// When `verify_full_hex_oid_exists` is false, a syntactically valid 40-hex OID is accepted even
/// if the object is missing (matches `git tag --points-at`). When true, a missing object is an
/// error (Grit `for-each-ref` regression coverage in t13070).
pub fn resolve_porcelain_points_at(
    repo: &Repository,
    spec: &str,
    verify_full_hex_oid_exists: bool,
) -> Result<ObjectId> {
    let oid = resolve_revision_without_index_dwim(repo, spec).map_err(|_| {
        anyhow::Error::new(ExplicitExit {
            code: 129,
            message: format!(
                "error: malformed object name {}",
                malformed_object_name_quoted(spec)
            ),
        })
    })?;

    if spec.len() == 40 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        if verify_full_hex_oid_exists && repo.odb.read(&oid).is_err() {
            return Err(anyhow::anyhow!("object {oid} not found"));
        }
        return Ok(oid);
    }

    if repo.odb.read(&oid).is_err() {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 129,
            message: format!(
                "error: malformed object name {}",
                malformed_object_name_quoted(spec)
            ),
        }));
    }

    Ok(oid)
}

/// Wrong-type / missing-object errors for `--merged` / `--no-merged` (exit 129).
fn merged_must_point_to_commit_exit(hex: &str, kind: ObjectKind) -> Result<ObjectId> {
    let kind_msg = match kind {
        ObjectKind::Tree => "tree",
        ObjectKind::Blob => "blob",
        ObjectKind::Tag => "tag",
        ObjectKind::Commit => "commit",
    };
    Err(anyhow::Error::new(ExplicitExit {
        code: 129,
        message: format!(
            "error: object {hex} is a {kind_msg}, not a commit\nerror: option `merged' must point to a commit"
        ),
    }))
}

/// Resolve a commit for `branch --merged` / `--no-merged` and `for-each-ref` merged filters.
///
/// Uses no index DWIM. Unresolvable names yield `fatal: malformed object name` (exit 128). Missing
/// full hex, missing peeled objects, or non-commit targets yield Git's `merged` option errors (exit
/// 129). Annotated tags peel to their target commit.
pub fn resolve_porcelain_merged_commit(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision_without_index_dwim(repo, spec).map_err(|_| {
        anyhow::Error::new(LibError::Message(format!(
            "fatal: malformed object name {spec}"
        )))
    })?;

    if spec.len() == 40
        && spec.chars().all(|c| c.is_ascii_hexdigit())
        && repo.odb.read(&oid).is_err()
    {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 129,
            message: "error: option `merged' must point to a commit".to_owned(),
        }));
    }

    let object = match repo.odb.read(&oid) {
        Ok(o) => o,
        Err(_) => {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 129,
                message: "error: option `merged' must point to a commit".to_owned(),
            }));
        }
    };

    match object.kind {
        ObjectKind::Commit => Ok(oid),
        ObjectKind::Tag => {
            let inner_oid = tag_object_line_oid(&object.data).ok_or_else(|| {
                anyhow::Error::new(ExplicitExit {
                    code: 129,
                    message: "error: option `merged' must point to a commit".to_owned(),
                })
            })?;
            let inner = match repo.odb.read(&inner_oid) {
                Ok(o) => o,
                Err(_) => {
                    return Err(anyhow::Error::new(ExplicitExit {
                        code: 129,
                        message: "error: option `merged' must point to a commit".to_owned(),
                    }));
                }
            };
            match inner.kind {
                ObjectKind::Commit => Ok(inner_oid),
                _ => merged_must_point_to_commit_exit(&inner_oid.to_hex(), inner.kind),
            }
        }
        _ => merged_must_point_to_commit_exit(&oid.to_hex(), object.kind),
    }
}
