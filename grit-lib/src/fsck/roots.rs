//! Reachability root collection for fsck and garbage collection.

use crate::error::Result;
use crate::index::{CacheTreeNode, Index, MODE_GITLINK};
use crate::objects::ObjectId;
use crate::reflog::{all_reflog_oids, reflog_oids_on_store};
use crate::refs;
use crate::repo::Repository;
use crate::resolve_undo::ResolveUndoRecord;
use crate::state::HeadState;
use crate::worktree::{list_worktrees, WorktreeEntry};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// Object ids that anchor connectivity and dangling-object classification.
///
/// Includes every ref tip, detached `HEAD` oids (repository and linked worktrees),
/// all reflog `old`/`new` oids from the repository ref store (including an injected
/// backend), plus linked-worktree admin-dir reflogs on disk,
/// every non-bare worktree index (main and linked, including prunable entries),
/// the `GIT_INDEX_FILE` index when set (in addition to each worktree's own index),
/// and the stash ref when present.
///
/// # Errors
///
/// Propagates ref, reflog, index, or worktree enumeration failures.
pub fn reachability_roots(repo: &Repository) -> Result<Vec<ObjectId>> {
    let git_dir = repo.git_dir();
    let mut roots: HashSet<ObjectId> = HashSet::new();

    for (_, oid) in refs::list_refs_in(repo, "refs/")? {
        roots.insert(oid);
    }

    if let Ok(oid) = refs::resolve_ref_in(repo, "HEAD") {
        roots.insert(oid);
    }

    if let Ok(oid) = refs::resolve_ref_in(repo, "refs/stash") {
        roots.insert(oid);
    }

    let worktrees = list_worktrees(repo)?;
    for wt in &worktrees {
        match &wt.head {
            HeadState::Detached { oid } => {
                roots.insert(*oid);
            }
            HeadState::Branch { oid: Some(oid), .. } => {
                roots.insert(*oid);
            }
            HeadState::Branch { .. } | HeadState::Invalid => {}
        }
    }

    roots.extend(reflog_oids_on_store(repo.refs())?);
    for wt in &worktrees {
        if wt.admin_dir != git_dir {
            roots.extend(all_reflog_oids(&wt.admin_dir)?);
        }
    }

    for wt in &worktrees {
        if wt.is_bare {
            continue;
        }
        let index = load_index_for_reachability(repo, wt)?;
        collect_index_object_roots(&index, &mut roots);
    }
    collect_git_index_file_roots(repo, &mut roots)?;

    Ok(roots.into_iter().collect())
}

/// Object ids recorded only in the index (and cache-tree), excluding gitlinks and zero ids.
///
/// Used to route missing-object diagnostics through the index fsck pass instead of the
/// connectivity walk (avoids duplicate `missingObject` reports for staged blobs).
///
/// Includes entries from every non-bare worktree index, not only the worktree the
/// [`Repository`] handle was opened from.
pub fn index_object_roots(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    for wt in list_worktrees(repo)? {
        if wt.is_bare {
            continue;
        }
        let index = load_index_for_reachability(repo, &wt)?;
        collect_index_object_roots(&index, &mut out);
    }
    collect_git_index_file_roots(repo, &mut out)?;
    Ok(out)
}

fn load_index_for_reachability(repo: &Repository, wt: &WorktreeEntry) -> Result<Index> {
    load_linked_worktree_index(repo, &wt.admin_dir)
}

fn collect_git_index_file_roots(repo: &Repository, out: &mut HashSet<ObjectId>) -> Result<()> {
    if repo.environment().git_index_file.is_none() {
        return Ok(());
    }
    let index = repo.load_index()?;
    collect_index_object_roots(&index, out);
    Ok(())
}

fn load_linked_worktree_index(repo: &Repository, admin_dir: &Path) -> Result<Index> {
    let index_path = admin_dir.join("index");
    let parse_ctx = crate::index::IndexParseContext::from_policy(repo.thread_policy());
    let cfg = repo
        .config()
        .unwrap_or_else(|_| Arc::new(crate::config::ConfigSet::new()));
    let mut idx = Index::load_preserving_sparse_optional_with_context(
        &index_path,
        repo.odb(),
        Some(cfg.as_ref()),
        parse_ctx,
    )?;
    crate::split_index::resolve_split_index_if_needed(&mut idx, admin_dir, &index_path)?;
    idx.expand_sparse_directory_placeholders(
        repo.odb(),
        crate::path_validate::ProtectOptions::from_config(cfg.as_ref()),
    )?;
    Ok(idx)
}

fn collect_index_object_roots(index: &Index, out: &mut HashSet<ObjectId>) {
    for entry in index.entries() {
        if entry.mode == MODE_GITLINK || entry.oid.is_zero() {
            continue;
        }
        out.insert(entry.oid);
    }
    if let Some(root) = index.cache_tree_root {
        out.insert(root);
    }
    if let Some(node) = &index.cache_tree {
        collect_cache_tree_oids(node, out);
    }
    if let Some(undo) = &index.resolve_undo {
        collect_resolve_undo_oids(undo, out);
    }
}

/// Whether a resolve-undo stage mode names a blob in this object database (Git `S_ISREG`).
fn is_reg_resolve_undo_mode(mode: u32) -> bool {
    mode != 0 && (mode & 0o170000) == 0o100000
}

fn collect_resolve_undo_oids(
    undo: &std::collections::BTreeMap<Vec<u8>, ResolveUndoRecord>,
    out: &mut HashSet<ObjectId>,
) {
    for record in undo.values() {
        for i in 0..3 {
            if is_reg_resolve_undo_mode(record.modes[i]) && !record.oids[i].is_zero() {
                out.insert(record.oids[i]);
            }
        }
    }
}

fn collect_cache_tree_oids(node: &CacheTreeNode, out: &mut HashSet<ObjectId>) {
    if let Some(oid) = node.oid {
        out.insert(oid);
    }
    for child in &node.children {
        collect_cache_tree_oids(child, out);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::ident::Signature;
    use crate::maintenance::repository_fsck::{fsck, FsckOptions};
    use crate::maintenance::{prune_loose_objects, PruneOptions};
    use crate::objects::odb::{store::MemoryStore, OdbBuilder};
    use crate::objects::{HashAlgo, NewCommit, ObjectKind};
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::refs::store::{
        Expected, MemoryRefStore, RawRef, RefTransaction, RefUpdate, ReflogUpdate,
    };
    use crate::repo::{InitOptions, Repository, RepositoryBuilder};
    use crate::worktree::{add_linked_worktree, AddLinkedWorktreeOptions, WorktreeBranch};
    use filetime::FileTime;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    fn commit_all(repo: &Repository, message: &str) {
        stage_for_commit(repo, &mut NullProgress, &NeverCancel).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_string();
        create_commit(
            repo,
            &CommitRequest {
                message: message.into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("commit");
    }

    #[test]
    fn reachability_roots_includes_index_staged_blob() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        commit_all(&repo, "c");
        std::fs::write(repo.work_tree().unwrap().join("staged-only"), b"y").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &crate::progress::NeverCancel).expect("stage2");
        let index = repo.load_index().expect("index");
        let staged = index
            .entries()
            .iter()
            .find(|e| e.path.ends_with(b"staged-only"))
            .expect("entry")
            .oid;
        let roots = reachability_roots(&repo).expect("roots");
        assert!(roots.contains(&staged));
    }

    #[test]
    fn reachability_roots_include_linked_worktree_index_from_main_and_linked_handles() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        commit_all(&repo, "base");

        let linked_path = dir.path().join("linked");
        add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked_path.clone(),
                branch: Some(WorktreeBranch::Create("linked".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .expect("linked");

        let linked_repo = Repository::discover(Some(&linked_path), &RepositoryOptions::empty())
            .expect("open linked");
        std::fs::write(linked_path.join("linked-only.txt"), b"linked staged").unwrap();
        stage_for_commit(&linked_repo, &mut NullProgress, &NeverCancel).expect("stage linked");
        let linked_index = linked_repo.load_index().expect("linked index");
        let linked_blob = linked_index
            .entries()
            .iter()
            .find(|e| e.path.ends_with(b"linked-only.txt"))
            .expect("linked entry")
            .oid;

        std::fs::write(
            repo.work_tree().unwrap().join("main-only.txt"),
            b"main staged",
        )
        .unwrap();
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage main");
        let main_index = repo.load_index().expect("main index");
        let main_blob = main_index
            .entries()
            .iter()
            .find(|e| e.path.ends_with(b"main-only.txt"))
            .expect("main entry")
            .oid;

        let roots_main = reachability_roots(&repo).expect("roots from main");
        assert!(roots_main.contains(&linked_blob));
        assert!(roots_main.contains(&main_blob));

        let roots_linked = reachability_roots(&linked_repo).expect("roots from linked");
        assert!(roots_linked.contains(&linked_blob));
        assert!(roots_linked.contains(&main_blob));
    }

    #[test]
    fn prune_keeps_blob_staged_only_in_linked_worktree_index() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        commit_all(&repo, "base");

        let linked_path = dir.path().join("linked");
        add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked_path.clone(),
                branch: Some(WorktreeBranch::Create("linked".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .expect("linked");

        let linked_repo = Repository::discover(Some(&linked_path), &RepositoryOptions::empty())
            .expect("open linked");
        std::fs::write(linked_path.join("s.txt"), b"staged only in linked worktree").unwrap();
        stage_for_commit(&linked_repo, &mut NullProgress, &NeverCancel).expect("stage");
        let blob = linked_repo
            .load_index()
            .expect("index")
            .entries()
            .iter()
            .find(|e| e.path.ends_with(b"s.txt"))
            .expect("entry")
            .oid;

        let loose = repo.git_dir().join("objects").join(format!(
            "{}/{}",
            &blob.to_hex()[..2],
            &blob.to_hex()[2..]
        ));
        assert!(loose.is_file());
        let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        filetime::set_file_mtime(&loose, FileTime::from_system_time(old)).unwrap();

        let mut opts = PruneOptions::default();
        opts.older_than = Some(SystemTime::now());
        let stats = prune_loose_objects(&repo, &opts).expect("prune");
        assert_eq!(stats.pruned, 0, "linked index blob must stay reachable");

        let status = Command::new("git")
            .current_dir(&linked_path)
            .args(["cat-file", "-e", &blob.to_hex()])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git cat-file");
        assert!(status.success(), "git must still read staged blob");

        let fsck = Command::new("git")
            .current_dir(&linked_path)
            .args(["fsck", "--strict"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git fsck");
        assert!(
            fsck.status.success(),
            "git fsck: {}",
            String::from_utf8_lossy(&fsck.stderr)
        );
    }

    #[test]
    fn prune_succeeds_with_gitlink_resolve_undo_from_index_info() {
        use std::io::Write;

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "--allow-empty", "-qm", "base"]);
        let index_info = "160000 1111111111111111111111111111111111111111 1\tsub\n\
                          160000 2222222222222222222222222222222222222222 2\tsub\n\
                          160000 3333333333333333333333333333333333333333 3\tsub\n";
        let mut child = Command::new("git")
            .current_dir(root)
            .args(["update-index", "--index-info"])
            .stdin(std::process::Stdio::piped())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .spawn()
            .expect("spawn index-info");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(index_info.as_bytes())
            .expect("write index-info");
        assert!(
            child.wait().expect("wait").success(),
            "git update-index --index-info failed"
        );
        git(&[
            "update-index",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,sub",
        ]);

        let repo =
            Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open grit");
        let gitlink =
            ObjectId::from_hex("1111111111111111111111111111111111111111").expect("gitlink oid");
        let roots = reachability_roots(&repo).expect("roots");
        assert!(
            !roots.contains(&gitlink),
            "submodule commit oids in REUC must not be superproject roots"
        );

        let mut opts = PruneOptions::default();
        opts.older_than = Some(SystemTime::now());
        prune_loose_objects(&repo, &opts).expect("prune with gitlink REUC");

        git(&["fsck", "--strict"]);
    }

    #[test]
    fn reachability_roots_tolerates_missing_linked_worktree_index() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        commit_all(&repo, "base");
        let linked_path = dir.path().join("linked");
        let entry = add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked_path,
                branch: Some(WorktreeBranch::Create("linked".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: true,
                track: false,
            },
        )
        .expect("linked");
        assert!(!entry.admin_dir.join("index").exists());
        reachability_roots(&repo).expect("roots with missing linked index");
        index_object_roots(&repo).expect("index roots with missing linked index");
    }

    #[test]
    fn injected_ref_store_reflog_commit_is_reachability_root() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join("mem.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let repo = RepositoryBuilder::new(RepositoryOptions::empty())
            .odb_builder(
                OdbBuilder::files(git_dir.join("objects"))
                    .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                    .alternates(false),
            )
            .ref_store(Arc::new(MemoryRefStore::new()))
            .initial_branch("main")
            .open(&git_dir, None)
            .unwrap();
        let sig = Signature::new(
            "T",
            "t@example.com",
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        )
        .unwrap();
        let tree = repo.odb().write(ObjectKind::Tree, &[]).unwrap();
        let c1 = repo
            .objects()
            .write_commit(&NewCommit::new(
                tree,
                sig.clone(),
                sig.clone(),
                "c1".to_string(),
            ))
            .unwrap();
        let c2 = repo
            .objects()
            .write_commit(&NewCommit::new(
                tree,
                sig.clone(),
                sig.clone(),
                "c2".to_string(),
            ))
            .unwrap();
        let ts = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let update = |new, exp, msg: &str| {
            let txn = RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/main".into(),
                    new_value: Some(RawRef::Direct(new)),
                    expected: exp,
                    flags: Default::default(),
                    reflog: Some(ReflogUpdate {
                        identity: "T <t@example.com>".into(),
                        message: msg.into(),
                        time: ts,
                        old_oid: None,
                        new_oid: None,
                    }),
                })
                .unwrap();
            repo.refs().prepare(txn).unwrap().commit().unwrap();
        };
        update(c1, Expected::Missing, "branch: Created");
        update(c2, Expected::Oid(c1), "reset: moving to c2");

        let roots = reachability_roots(&repo).unwrap();
        assert!(roots.contains(&c1), "reflog-only commit must be a root");

        let report = fsck(
            &repo,
            &FsckOptions::default().with_check_dangling(true),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert!(
            !report.dangling.iter().any(|(_, oid)| *oid == c1),
            "reflog-only commit must not be dangling: {:?}",
            report.dangling
        );
    }
}
