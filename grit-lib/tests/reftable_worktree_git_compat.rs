//! Per-worktree reftable HEAD interoperability with system Git (upstream t0610 layout).

mod support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::refs::store::RawRef;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::state::{resolve_head, resolve_head_for_repository, HeadState};
use grit_lib::worktree::{add_linked_worktree, AddLinkedWorktreeOptions, WorktreeBranch};
use grit_lib::PostOperationHooks;
use grit_test_support::git;

fn open(path: &Path) -> Repository {
    Repository::discover(Some(path), &RepositoryOptions::empty()).expect("open repository")
}

fn seed(path: &Path) -> ObjectId {
    git(path, &["config", "user.name", "Tester"]);
    git(path, &["config", "user.email", "tester@example.org"]);
    fs::write(path.join("a"), "base\n").unwrap();
    git(path, &["add", "a"]);
    git(path, &["commit", "-qm", "base"]);
    git(path, &["rev-parse", "HEAD"]).trim().parse().unwrap()
}

fn assert_head(path: &Path, oid: ObjectId, branch: Option<&str>, status: &str) {
    assert_eq!(git(path, &["rev-parse", "HEAD"]).trim(), oid.to_hex());
    let repo = open(path);
    assert_eq!(repo.refs().resolve("HEAD").unwrap(), oid);
    let expected = branch.map_or(HeadState::Detached { oid }, |name| HeadState::Branch {
        refname: format!("refs/heads/{name}"),
        short_name: name.into(),
        oid: Some(oid),
    });
    assert_eq!(resolve_head_for_repository(&repo).unwrap(), expected);
    assert_eq!(resolve_head(repo.git_dir()).unwrap(), expected);
    let list = git(path, &["worktree", "list", "--porcelain"]);
    let worktree_line = format!("worktree {}", path.canonicalize().unwrap().display());
    let row = list
        .split("\n\n")
        .find(|row| row.lines().next() == Some(worktree_line.as_str()))
        .expect("worktree row");
    assert!(row.contains(&format!("HEAD {oid}")), "{row}");
    if let Some(branch) = branch {
        let target = format!("refs/heads/{branch}");
        assert_eq!(git(path, &["symbolic-ref", "HEAD"]).trim(), target);
        assert_eq!(
            repo.refs().read_raw("HEAD").unwrap(),
            Some(RawRef::Symbolic(target.clone()))
        );
        assert!(row.contains(&format!("branch {target}")), "{row}");
    } else {
        assert_eq!(
            repo.refs().read_raw("HEAD").unwrap(),
            Some(RawRef::Direct(oid))
        );
        assert!(!std::process::Command::new("git")
            .current_dir(path)
            .args(["symbolic-ref", "-q", "HEAD"])
            .status()
            .unwrap()
            .success());
        assert!(row.contains("\ndetached"), "{row}");
    }
    assert_eq!(git(path, &["status", "--short"]), status);
    git(path, &["fsck", "--strict"]);
}

fn switch(path: &Path, mode: SwitchMode) {
    switch_repository(
        &open(path),
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap();
}

#[test]
fn reftable_add_attached_detached_and_no_checkout_are_git_readable() {
    if !support::require_reftable_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("repo");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", "--ref-format=reftable", "repo"],
    );
    let base = seed(&main);
    let sibling = tmp.path().join("sibling");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "sibling",
            sibling.to_str().unwrap(),
        ],
    );
    for (name, detached, no_checkout) in [
        ("attached", false, false),
        ("detached", true, false),
        ("attached-empty", false, true),
        ("detached-empty", true, true),
    ] {
        if !detached {
            git(&main, &["branch", name]);
        }
        let path = tmp.path().join(name);
        let repo = open(&main);
        let entry = add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: path.clone(),
                branch: Some(if detached {
                    WorktreeBranch::Detach
                } else {
                    WorktreeBranch::Existing(name.into())
                }),
                start_point: Some(if detached {
                    base.to_hex()
                } else {
                    name.to_owned()
                }),
                force: false,
                lock: None,
                no_checkout,
                track: false,
            },
        )
        .unwrap();
        assert!(entry.admin_dir.join("reftable/tables.list").is_file());
        assert!(entry.admin_dir.join("refs/heads").is_file());
        assert_eq!(
            fs::read_to_string(entry.admin_dir.join("HEAD")).unwrap(),
            "ref: refs/heads/.invalid\n"
        );
        if no_checkout {
            assert!(!path.join("a").exists());
            assert!(!entry.admin_dir.join("index").exists());
        }
        assert_head(
            &path,
            base,
            (!detached).then_some(name),
            if no_checkout { "D  a\n" } else { "" },
        );
        if no_checkout {
            // Git can materialize the grit-created worktree without repairing its HEAD.
            git(&path, &["reset", "--hard", "HEAD"]);
            assert_head(&path, base, (!detached).then_some(name), "");
        }
        assert_head(&main, base, Some("main"), "");
        assert_head(&sibling, base, Some("sibling"), "");
    }
}

#[test]
fn git_created_reftable_worktree_switch_attach_detach_roundtrips() {
    if !support::require_reftable_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("repo");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", "--ref-format=reftable", "repo"],
    );
    let base = seed(&main);
    let path = tmp.path().join("git-wt");
    git(
        &main,
        &["worktree", "add", "-b", "topic", path.to_str().unwrap()],
    );
    fs::write(path.join("a"), "topic edit\n").unwrap();
    git(&path, &["commit", "-qam", "topic-edit"]);
    let topic: ObjectId = git(&path, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    git(&main, &["branch", "switch-target", "main"]);
    let sibling = tmp.path().join("sibling");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "sibling",
            sibling.to_str().unwrap(),
        ],
    );

    switch(
        &path,
        SwitchMode::Existing {
            branch: "switch-target".into(),
            oid: base,
        },
    );
    assert_head(&path, base, Some("switch-target"), "");
    // An old loose HEAD must not override the worktree stack when alternating tools.
    fs::write(
        open(&path).git_dir().join("HEAD"),
        "ref: refs/heads/topic\n",
    )
    .unwrap();
    assert_head(&path, base, Some("switch-target"), "");
    switch(
        &path,
        SwitchMode::Detached {
            commit: topic,
            reflog_to_label: topic.to_hex(),
        },
    );
    assert_head(&path, topic, None, "");
    switch(
        &path,
        SwitchMode::Existing {
            branch: "topic".into(),
            oid: topic,
        },
    );
    assert_head(&path, topic, Some("topic"), "");
    // Reverse direction: Git updates the same stack, and a fresh library handle sees it.
    git(&path, &["switch", "switch-target"]);
    assert_head(&path, base, Some("switch-target"), "");
    git(&path, &["switch", "--detach", &topic.to_hex()]);
    assert_head(&path, topic, None, "");
    assert_head(&main, base, Some("main"), "");
    assert_head(&sibling, base, Some("sibling"), "");
    assert_eq!(git(&main, &["rev-parse", "topic"]).trim(), topic.to_hex());
    assert_eq!(
        git(&main, &["rev-parse", "switch-target"]).trim(),
        base.to_hex()
    );
}

#[test]
fn grit_initialized_reftable_head_is_visible_to_git_before_and_after_commit() {
    if !support::require_reftable_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .unwrap();
    assert_eq!(
        git(tmp.path(), &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    let base = seed(tmp.path());
    assert_head(tmp.path(), base, Some("main"), "");
}
