//! Commit subject lines for history rewrite outcomes.

use crate::error::Error;
use crate::objects::{parse_commit, CommitData, ObjectId, ObjectKind};
use crate::porcelain::rebase::commit_subject_single_line;
use crate::repo::Repository;

use super::error::HistoryError;
use super::types::RewrittenCommit;

fn library_err(err: Error) -> HistoryError {
    HistoryError::Library(Box::new(err))
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData, HistoryError> {
    let raw = repo.odb().read(&oid).map_err(library_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(library_err(Error::CorruptObject(format!(
            "expected commit, got {}",
            raw.kind.as_str()
        ))));
    }
    parse_commit(&raw.data).map_err(library_err)
}

/// First line of a commit message (Git subject), without trailing newline.
#[must_use]
pub fn commit_message_subject_line(message: &str) -> String {
    commit_subject_single_line(message)
}

/// Subject for one rewritten row while planning: replacement message, or the original when dropped.
pub(crate) fn subject_for_rewrite_row(
    repo: &Repository,
    old: &CommitData,
    new: Option<ObjectId>,
) -> Result<Option<String>, HistoryError> {
    Ok(Some(if let Some(new) = new {
        commit_subject_single_line(&load_commit(repo, new)?.message)
    } else {
        commit_subject_single_line(&old.message)
    }))
}

/// Subject for a [`RewrittenCommit`] row (uses stored subject when present).
///
/// # Errors
///
/// Returns [`super::HistoryError`] when a replacement commit cannot be read from the object database.
pub fn rewritten_row_subject(
    repo: &Repository,
    row: &RewrittenCommit,
) -> Result<Option<String>, HistoryError> {
    if let Some(subject) = row.subject.as_ref() {
        return Ok(Some(subject.clone()));
    }
    if row.new.is_none() {
        let old = load_commit(repo, row.old)?;
        return Ok(Some(commit_subject_single_line(&old.message)));
    }
    let new = row.new.ok_or(HistoryError::Planning {
        message: "rewrite row missing replacement oid".into(),
    })?;
    let commit = load_commit(repo, new)?;
    Ok(Some(commit_subject_single_line(&commit.message)))
}

#[cfg(test)]
mod tests {
    use crate::history::{
        drop as history_drop, reword, EmptyPolicy, RefScope, RewriteOptions, RewritePlan,
    };
    use crate::objects::{serialize_commit, CommitData};
    use crate::repo::init_repository;
    use crate::RefStorageFormat;

    use super::*;

    fn committer(n: i64) -> String {
        format!("Committer <c@example.com> {n} +0000")
    }

    fn write_commit(
        repo: &Repository,
        tree: ObjectId,
        parents: &[ObjectId],
        message: &str,
    ) -> ObjectId {
        let bytes = serialize_commit(&CommitData {
            tree,
            parents: parents.to_vec(),
            author: "Author <a@example.com> 1 +0000".into(),
            committer: committer(2),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: message.into(),
            raw_message: None,
            extra_headers: vec![],
        });
        repo.odb()
            .write(ObjectKind::Commit, &bytes)
            .expect("write commit")
    }

    fn linear_repo() -> (tempfile::TempDir, Repository, ObjectId, ObjectId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let tree = repo.odb().write(ObjectKind::Tree, &[]).expect("tree");
        let a = write_commit(&repo, tree, &[], "first subject\n");
        let b = write_commit(&repo, tree, &[a], "second subject\n");
        let c = write_commit(&repo, tree, &[b], "third subject\n");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &c).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");
        (dir, repo, b, c)
    }

    fn rewrite_opts() -> RewriteOptions {
        RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(3),
            preserve_committer: false,
            sign: false,
        }
    }

    fn row_subjects(repo: &Repository, plan: &RewritePlan) -> Vec<Option<String>> {
        plan.subjects(repo).expect("subjects")
    }

    #[test]
    fn rewrite_row_subject_uses_replacement_message() {
        let (_dir, repo, middle, _tip) = linear_repo();
        let plan =
            reword(&repo, middle, "reworded subject\n".into(), &rewrite_opts()).expect("reword");
        let row = plan
            .commits
            .iter()
            .find(|r| r.old == middle)
            .expect("middle row");
        assert_eq!(row.subject.as_deref(), Some("reworded subject"));
        let idx = plan.commits.iter().position(|r| r.old == middle).unwrap();
        assert_eq!(
            row_subjects(&repo, &plan)[idx].as_deref(),
            Some("reworded subject")
        );
        assert_eq!(
            rewritten_row_subject(&repo, row).expect("lookup"),
            Some("reworded subject".into())
        );
    }

    #[test]
    fn dropped_row_subject_uses_original_message() {
        let (_dir, repo, middle, _tip) = linear_repo();
        let plan = history_drop(&repo, middle, &rewrite_opts()).expect("drop");
        let row = plan
            .commits
            .iter()
            .find(|r| r.old == middle)
            .expect("dropped row");
        assert!(row.new.is_none());
        assert_eq!(row.subject.as_deref(), Some("second subject"));
        assert_eq!(
            rewritten_row_subject(&repo, row).expect("lookup"),
            Some("second subject".into())
        );
    }
}
