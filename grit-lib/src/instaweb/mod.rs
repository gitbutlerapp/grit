//! Data for a lightweight repository web UI (`git instaweb`).
//!
//! Handlers stay free of HTML or HTTP; the CLI / server crate renders pages.

use crate::commit_pretty::{abbrev_hex, message_subject};
use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind, TreeEntry};
use crate::refs::{list_refs, read_head, resolve_ref};
use crate::repo::Repository;
use crate::rev_list::{rev_list, OrderingMode, RevListOptions};
use std::str::FromStr;

/// Summary information shown on the project home page.
#[derive(Debug, Clone)]
pub struct RepoSummary {
    /// Display name (work tree directory or bare repo folder name).
    pub name: String,
    /// Short description from `repository.description` config, if set.
    pub description: Option<String>,
    /// Current `HEAD` ref name, if symbolic.
    pub head_ref: Option<String>,
    /// Object id `HEAD` resolves to.
    pub head_oid: Option<ObjectId>,
}

/// One ref row for branch/tag lists.
#[derive(Debug, Clone)]
pub struct RefRow {
    /// Full ref name (e.g. `refs/heads/main`).
    pub name: String,
    /// Tip object id.
    pub oid: ObjectId,
}

/// One commit row for the recent-log table.
#[derive(Debug, Clone)]
pub struct CommitRow {
    /// Commit object id.
    pub oid: ObjectId,
    /// Abbreviated id for display.
    pub abbrev: String,
    /// One-line subject.
    pub subject: String,
    /// Author line from the commit object.
    pub author: String,
}

/// Full commit view for the commit detail page.
#[derive(Debug, Clone)]
pub struct CommitView {
    /// Commit object id.
    pub oid: ObjectId,
    /// Parsed commit headers and message.
    pub data: crate::objects::CommitData,
    /// Abbreviated id for display.
    pub abbrev: String,
    /// One-line subject.
    pub subject: String,
}

/// One tree listing row.
#[derive(Debug, Clone)]
pub struct TreeRow {
    /// Mode formatted like `git ls-tree` (e.g. `100644`, `40000`).
    pub mode: String,
    /// Entry kind for linking (`blob`, `tree`, …).
    pub kind: TreeEntryKind,
    /// File or directory name (UTF-8 lossy).
    pub name: String,
    /// Object id.
    pub oid: ObjectId,
}

/// How to link a tree entry in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEntryKind {
    /// Sub-tree (`40000` mode).
    Tree,
    /// Regular file blob.
    Blob,
    /// Other gitlink/submodule modes.
    Other,
}

/// Blob payload for the blob view page.
#[derive(Debug, Clone)]
pub struct BlobView {
    /// Object id.
    pub oid: ObjectId,
    /// Whether the payload is valid UTF-8 text.
    pub is_text: bool,
    /// Text content when [`Self::is_text`] is true.
    pub text: String,
    /// Raw size in bytes.
    pub size: usize,
    /// True when content was truncated for display.
    pub truncated: bool,
}

/// Load repository summary for the instaweb home page.
///
/// # Errors
///
/// Returns [`Error::NotARepository`] when `repo` is invalid, or object/ref errors from resolution.
pub fn load_summary(repo: &Repository) -> Result<RepoSummary> {
    let name = repo
        .work_tree
        .as_ref()
        .or(Some(&repo.git_dir))
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("repository")
        .to_owned();

    let config = crate::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let description = config
        .get("repository.description")
        .filter(|s| !s.is_empty());

    let head_ref = read_head(&repo.git_dir)?;
    let head_oid = head_ref
        .as_ref()
        .filter(|r| r.starts_with("ref: "))
        .and_then(|r| r.strip_prefix("ref: "))
        .map(str::trim)
        .and_then(|refname| resolve_ref(&repo.git_dir, refname).ok())
        .or_else(|| {
            head_ref
                .as_ref()
                .filter(|h| !h.starts_with("ref: "))
                .and_then(|h| ObjectId::from_str(h.trim()).ok())
        });

    Ok(RepoSummary {
        name,
        description,
        head_ref: head_ref.and_then(|h| {
            h.strip_prefix("ref: ")
                .map(|r| r.trim().to_owned())
                .or(Some(h))
        }),
        head_oid,
    })
}

/// List branches and tags for display.
///
/// # Errors
///
/// Propagates ref listing failures from [`list_refs`].
pub fn list_display_refs(repo: &Repository) -> Result<Vec<RefRow>> {
    let mut out = Vec::new();
    for prefix in ["refs/heads/", "refs/tags/"] {
        for (name, oid) in list_refs(&repo.git_dir, prefix)? {
            out.push(RefRow { name, oid });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Walk `HEAD` history and return up to `limit` commits (newest first).
///
/// # Errors
///
/// Returns revision walk or object parse errors.
pub fn recent_commits(repo: &Repository, limit: usize) -> Result<Vec<CommitRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let opts = RevListOptions {
        max_count: Some(limit),
        ordering: OrderingMode::Topo,
        ..RevListOptions::default()
    };
    let result = match rev_list(repo, &["HEAD".to_owned()], &[], &opts) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    let mut rows = Vec::new();
    for oid in result.commits {
        let commit = load_commit(repo, &oid)?;
        rows.push(CommitRow {
            oid,
            abbrev: commit.abbrev,
            subject: commit.subject,
            author: commit.data.author.clone(),
        });
    }
    Ok(rows)
}

/// Load a commit object by id.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] or corrupt object errors.
pub fn load_commit(repo: &Repository, oid: &ObjectId) -> Result<CommitView> {
    let obj = repo.odb.read(oid)?;
    let commit = parse_commit(&obj.data)?;
    let abbrev = abbrev_hex(oid, 7);
    let subject = message_subject(&commit.message);
    Ok(CommitView {
        oid: *oid,
        data: commit,
        abbrev,
        subject,
    })
}

/// Resolve `spec` to a commit id (peels annotated tags one level).
///
/// # Errors
///
/// Returns parse or object errors when `spec` does not resolve.
pub fn resolve_commitish(repo: &Repository, spec: &str) -> Result<ObjectId> {
    if let Ok(oid) = ObjectId::from_str(spec) {
        return peel_to_commit(repo, &oid);
    }
    let oid = resolve_ref(&repo.git_dir, spec)?;
    peel_to_commit(repo, &oid)
}

/// Peel annotated tags (and pass commits through) for instaweb commit links.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when `oid` is not a commit or tag object.
pub fn peel_to_commit(repo: &Repository, oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(oid)?;
    match obj.kind {
        ObjectKind::Commit => Ok(*oid),
        ObjectKind::Tag => {
            let tag = parse_tag(&obj.data)?;
            peel_to_commit(repo, &tag.object)
        }
        other => Err(Error::CorruptObject(format!(
            "expected commit for instaweb view, got {other:?}"
        ))),
    }
}

/// List entries of a tree object, sorted Git-style.
///
/// # Errors
///
/// Returns object read/parse failures.
pub fn load_tree(repo: &Repository, oid: &ObjectId) -> Result<Vec<TreeRow>> {
    let obj = repo.odb.read(oid)?;
    let mut entries = parse_tree(&obj.data)?;
    entries.sort_by(|a, b| {
        crate::objects::tree_entry_cmp(&a.name, a.mode == 0o040000, &b.name, b.mode == 0o040000)
    });
    Ok(entries.into_iter().map(|e| tree_entry_to_row(e)).collect())
}

fn tree_entry_to_row(entry: TreeEntry) -> TreeRow {
    let kind = if entry.mode == 0o040000 {
        TreeEntryKind::Tree
    } else if entry.mode & 0o170000 == 0o100000 {
        TreeEntryKind::Blob
    } else {
        TreeEntryKind::Other
    };
    TreeRow {
        mode: TreeEntry {
            mode: entry.mode,
            name: entry.name.clone(),
            oid: entry.oid,
        }
        .mode_str(),
        kind,
        name: String::from_utf8_lossy(&entry.name).into_owned(),
        oid: entry.oid,
    }
}

/// Read blob contents for display, truncating at `max_bytes`.
///
/// # Errors
///
/// Returns object read failures or non-blob kinds.
pub fn load_blob(repo: &Repository, oid: &ObjectId, max_bytes: usize) -> Result<BlobView> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != ObjectKind::Blob {
        return Err(Error::CorruptObject(format!(
            "instaweb blob view expected blob, got {:?}",
            obj.kind
        )));
    }
    let size = obj.data.len();
    let truncated = size > max_bytes;
    let slice = if truncated {
        &obj.data[..max_bytes]
    } else {
        &obj.data[..]
    };
    let is_text = std::str::from_utf8(slice).is_ok();
    let text = if is_text {
        String::from_utf8_lossy(slice).into_owned()
    } else {
        String::new()
    };
    Ok(BlobView {
        oid: *oid,
        is_text,
        text,
        size,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{
        serialize_commit, serialize_tag, serialize_tree, CommitData, ObjectKind, TagData, TreeEntry,
    };
    use crate::refs;
    use std::fs;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::create_dir_all(git_dir.join("refs/tags")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        let repo = Repository::discover(Some(tmp.path())).unwrap();
        (tmp, repo)
    }

    fn write_sample_commit(repo: &Repository) -> ObjectId {
        let tree_oid = repo.odb.write(ObjectKind::Tree, &[]).unwrap();
        let commit = CommitData {
            tree: tree_oid,
            parents: vec![],
            author: "Author <a@example.com>".to_owned(),
            committer: "Committer <c@example.com>".to_owned(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "subject line\n".to_owned(),
            raw_message: None,
        };
        repo.odb
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .unwrap()
    }

    #[test]
    fn load_summary_uses_work_tree_name() {
        let (tmp, repo) = init_repo();
        let summary = load_summary(&repo).unwrap();
        assert_eq!(
            summary.name,
            tmp.path().file_name().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn load_summary_reads_description() {
        let (tmp, _repo) = init_repo();
        fs::write(
            tmp.path().join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n[repository]\n\tdescription = hello\n",
        )
        .unwrap();
        let repo = Repository::discover(Some(tmp.path())).unwrap();
        let summary = load_summary(&repo).unwrap();
        assert_eq!(summary.description.as_deref(), Some("hello"));
    }

    #[test]
    fn list_display_refs_includes_heads_and_tags() {
        let (_tmp, repo) = init_repo();
        let commit = write_sample_commit(&repo);
        refs::write_ref(&repo.git_dir, "refs/heads/main", &commit).unwrap();
        let tag = TagData {
            object: commit,
            object_type: "commit".to_owned(),
            tag: "v1".to_owned(),
            tagger: None,
            message: String::new(),
        };
        let tag_oid = repo
            .odb
            .write(ObjectKind::Tag, &serialize_tag(&tag))
            .unwrap();
        refs::write_ref(&repo.git_dir, "refs/tags/v1", &tag_oid).unwrap();
        let rows = list_display_refs(&repo).unwrap();
        assert!(rows.iter().any(|r| r.name == "refs/heads/main"));
        assert!(rows.iter().any(|r| r.name == "refs/tags/v1"));
    }

    #[test]
    fn recent_commits_returns_subject() {
        let (_tmp, repo) = init_repo();
        let commit = write_sample_commit(&repo);
        refs::write_ref(&repo.git_dir, "refs/heads/main", &commit).unwrap();
        fs::write(repo.git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let rows = recent_commits(&repo, 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject, "subject line");
    }

    #[test]
    fn peel_to_commit_through_annotated_tag() {
        let (_tmp, repo) = init_repo();
        let commit = write_sample_commit(&repo);
        let tag = TagData {
            object: commit,
            object_type: "commit".to_owned(),
            tag: "v1".to_owned(),
            tagger: None,
            message: String::new(),
        };
        let tag_oid = repo
            .odb
            .write(ObjectKind::Tag, &serialize_tag(&tag))
            .unwrap();
        assert_eq!(peel_to_commit(&repo, &tag_oid).unwrap(), commit);
        assert_eq!(resolve_commitish(&repo, &tag_oid.to_hex()).unwrap(), commit);
    }

    #[test]
    fn load_tree_and_blob_round_trip() {
        let (_tmp, repo) = init_repo();
        let blob_oid = repo.odb.write(ObjectKind::Blob, b"hello\n").unwrap();
        let tree_bytes = serialize_tree(&[TreeEntry {
            mode: 0o100644,
            name: b"hello.txt".to_vec(),
            oid: blob_oid,
        }]);
        let tree_oid = repo.odb.write(ObjectKind::Tree, &tree_bytes).unwrap();
        let rows = load_tree(&repo, &tree_oid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "hello.txt");
        let blob = load_blob(&repo, &blob_oid, 1024).unwrap();
        assert!(blob.is_text);
        assert_eq!(blob.text, "hello\n");
    }
}
