//! `RepositoryStash::apply_commit` parity with system `git stash apply` (porcelain + file bytes).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::stash::StashApplyOptions;
use grit_test_support::git;

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn copy_repo(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir dest");
    let status = std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", from.display()),
            &to.to_string_lossy(),
        ])
        .status()
        .expect("cp");
    assert!(status.success(), "cp -a failed");
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

#[test]
fn apply_stash_without_index_matches_git_when_head_unchanged() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    std::fs::write(base.path().join("x"), "committed\n").expect("write");
    git_cmd(base.path(), &["add", "x"]);
    git_cmd(base.path(), &["commit", "-qm", "init"]);
    let head = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    std::fs::write(base.path().join("x"), "stashed content\n").expect("modify");
    git_cmd(base.path(), &["stash", "push", "-qm", "bench"]);
    let stash_oid_hex = git_cmd(base.path(), &["rev-parse", "refs/stash"])
        .trim()
        .to_owned();
    git_cmd(base.path(), &["reset", "--hard", &head]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    let grit_repo = Repository::open(
        &grit_dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(grit_dir.path()),
    )
    .expect("open");
    let stash_oid = ObjectId::from_hex(&stash_oid_hex).expect("stash oid");
    grit_repo
        .stash()
        .apply_commit(
            &stash_oid,
            &StashApplyOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("apply");

    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git_cmd(git_dir.path(), &["stash", "apply", "stash@{0}"]);

    let grit_porcelain = git_cmd(grit_dir.path(), &["status", "--porcelain"]);
    let git_porcelain = git_cmd(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(
        grit_porcelain, git_porcelain,
        "grit and git porcelain must match for apply without --index"
    );
    assert!(
        grit_porcelain.contains("x"),
        "expected x in porcelain (git shows unstaged modify), got:\n{grit_porcelain}"
    );

    let grit_bytes = std::fs::read(grit_dir.path().join("x")).expect("read grit x");
    let git_bytes = std::fs::read(git_dir.path().join("x")).expect("read git x");
    assert_eq!(grit_bytes, git_bytes);
    assert_eq!(grit_bytes, b"stashed content\n");
}

#[test]
fn apply_stash_with_index_matches_git_staged_porcelain() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    std::fs::write(base.path().join("y"), "base\n").expect("write");
    git_cmd(base.path(), &["add", "y"]);
    git_cmd(base.path(), &["commit", "-qm", "init"]);
    let head = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    std::fs::write(base.path().join("y"), "staged stash\n").expect("modify");
    git_cmd(base.path(), &["add", "y"]);
    git_cmd(base.path(), &["stash", "push", "-qm", "bench"]);
    let stash_oid_hex = git_cmd(base.path(), &["rev-parse", "refs/stash"])
        .trim()
        .to_owned();
    git_cmd(base.path(), &["reset", "--hard", &head]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    let grit_repo = Repository::open(
        &grit_dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(grit_dir.path()),
    )
    .expect("open");
    grit_repo
        .stash()
        .apply_commit(
            &ObjectId::from_hex(&stash_oid_hex).expect("stash"),
            &StashApplyOptions {
                restore_index: true,
                ..Default::default()
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("apply");

    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git_cmd(git_dir.path(), &["stash", "apply", "--index", "stash@{0}"]);

    let grit_porcelain = git_cmd(grit_dir.path(), &["status", "--porcelain"]);
    let git_porcelain = git_cmd(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(grit_porcelain, git_porcelain);
    assert!(
        !grit_porcelain.trim().is_empty(),
        "staged stash apply should leave staged entries in porcelain"
    );
    assert_eq!(
        std::fs::read(grit_dir.path().join("y")).expect("grit y"),
        b"staged stash\n"
    );
}

/// Exercise sparse deltas over many identical subtrees, including index-only changes.
#[test]
fn wide_sparse_apply_preserves_index_and_untouched_changes_like_git() {
    let base = tempfile::tempdir().expect("base");
    init_repo(base.path());
    for d in 0..40 {
        let dir = base.path().join(format!("d{d:03}"));
        std::fs::create_dir(&dir).expect("directory");
        for f in 0..100 {
            std::fs::write(dir.join(format!("f{f:03}")), format!("base {d} {f}\n")).expect("file");
        }
    }
    git_cmd(base.path(), &["add", "."]);
    git_cmd(base.path(), &["commit", "-qm", "wide base"]);
    let head = git_cmd(base.path(), &["rev-parse", "HEAD"]);
    for f in 0..30 {
        std::fs::write(
            base.path().join(format!("d000/f{f:03}")),
            format!("stash {f}\n"),
        )
        .expect("edit");
    }
    std::fs::write(base.path().join("d001/added"), "staged addition\n").expect("add");
    git_cmd(base.path(), &["add", "d001/added", "d000/f000"]);
    git_cmd(base.path(), &["rm", "-q", "d002/f000"]);
    // Worktree and index versions differ, and one staged addition has no worktree delta.
    std::fs::write(base.path().join("d000/f000"), "unstaged after staged\n").expect("edit");
    std::fs::write(base.path().join("d001/index-only"), "index only\n").expect("add");
    git_cmd(base.path(), &["add", "d001/index-only"]);
    std::fs::remove_file(base.path().join("d001/index-only")).expect("remove");
    git_cmd(base.path(), &["stash", "push", "-qm", "sparse"]);
    let stash = ObjectId::from_hex(git_cmd(base.path(), &["rev-parse", "refs/stash"]).trim())
        .expect("stash");

    for moved in [false, true] {
        for restore_index in [false, true] {
            let grit_dir = tempfile::tempdir().expect("grit");
            copy_repo(base.path(), grit_dir.path());
            if moved {
                std::fs::write(grit_dir.path().join("d010/f000"), "new head\n").expect("head edit");
                git_cmd(grit_dir.path(), &["add", "d010/f000"]);
                git_cmd(grit_dir.path(), &["commit", "-qm", "moved head"]);
            }
            std::fs::write(grit_dir.path().join("d039/f099"), "untouched local\n").expect("local");
            if !restore_index {
                std::fs::write(grit_dir.path().join("d039/f098"), "untouched staged\n")
                    .expect("local staged");
                git_cmd(grit_dir.path(), &["add", "d039/f098"]);
            }
            let git_dir = tempfile::tempdir().expect("git");
            copy_repo(grit_dir.path(), git_dir.path());
            let repo = Repository::open(
                &grit_dir.path().join(".git"),
                &RepositoryOptions::empty().with_work_tree(grit_dir.path()),
            )
            .expect("open");
            let outcome = repo
                .stash()
                .apply_commit(
                    &stash,
                    &StashApplyOptions {
                        restore_index,
                        ..Default::default()
                    },
                    &mut grit_lib::progress::NullProgress,
                    &grit_lib::progress::NeverCancel,
                )
                .expect("apply");
            assert!(!outcome.has_conflicts());
            let mut args = vec!["stash", "apply"];
            if restore_index {
                args.push("--index");
            }
            git_cmd(git_dir.path(), &args);
            for args in [
                &["diff", "--binary"][..],
                &["diff", "--cached", "--binary"][..],
                &["ls-files", "--stage"][..],
                &["status", "--porcelain"][..],
            ] {
                assert_eq!(
                    git_cmd(grit_dir.path(), args),
                    git_cmd(git_dir.path(), args),
                    "moved={moved}, index={restore_index}, {args:?}"
                );
            }
            for root in [grit_dir.path(), git_dir.path()] {
                git_cmd(root, &["fsck", "--strict"]);
                assert_eq!(
                    std::fs::read(root.join("d039/f099")).expect("local"),
                    b"untouched local\n"
                );
            }
        }
    }
    assert_eq!(git_cmd(base.path(), &["rev-parse", "HEAD"]), head);
}

#[cfg(unix)]
mod raw_stash_paths {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    fn paths() -> Vec<PathBuf> {
        [
            b"grit+pathhex:plain.txt".as_slice(),
            b"grit+pathhex:6162/nested.txt".as_slice(),
            b"nested/non-utf8-\xff.txt".as_slice(),
        ]
        .into_iter()
        .map(|bytes| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
        .collect()
    }

    fn setup(root: &Path, path: &Path) {
        init_repo(root);
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::write(root.join(path), b"base\n").unwrap();
        std::fs::write(root.join("untouched"), b"unchanged\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "base"]);
    }

    fn open(root: &Path) -> Repository {
        Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap()
    }

    fn assert_state_matches_git(actual: &Path, expected: &Path, path: &Path) {
        assert_eq!(
            std::fs::read(actual.join(path)).ok(),
            std::fs::read(expected.join(path)).ok(),
            "worktree bytes for {path:?}"
        );
        for args in [
            vec!["ls-files", "--stage"],
            vec!["status", "--porcelain=v1", "--untracked-files=all"],
            vec!["diff", "--binary"],
            vec!["diff", "--cached", "--binary"],
            vec!["stash", "list", "--format=%H"],
        ] {
            assert_eq!(git_cmd(actual, &args), git_cmd(expected, &args), "{args:?}");
        }
        assert_eq!(std::fs::read(actual.join("untouched")).unwrap(), b"local\n");
        git_cmd(actual, &["fsck", "--strict"]);
        git_cmd(expected, &["fsck", "--strict"]);
    }

    #[test]
    fn pop_rejects_local_overwrite_and_keeps_raw_path_stash() {
        for path in paths() {
            let root = tempfile::tempdir().unwrap();
            setup(root.path(), &path);
            std::fs::write(root.path().join(&path), b"stash\n").unwrap();
            git_cmd(root.path(), &["stash", "push", "-qm", "edit"]);
            std::fs::write(root.path().join(&path), b"precious local\n").unwrap();
            std::fs::write(root.path().join("untouched"), b"local\n").unwrap();
            let expected = tempfile::tempdir().unwrap();
            copy_repo(root.path(), expected.path());
            let before_index = std::fs::read(root.path().join(".git/index")).unwrap();
            let repo = open(root.path());
            let stash_oid =
                ObjectId::from_hex(git_cmd(root.path(), &["rev-parse", "refs/stash"]).trim())
                    .unwrap();
            let commit = repo.odb().read_commit(&stash_oid).unwrap();
            assert!(
                grit_lib::stash::check_stash_apply_would_overwrite_local_changes(
                    &repo,
                    root.path(),
                    &commit
                )
                .is_err(),
                "public preflight must reject {path:?}"
            );
            assert!(matches!(
                repo.stash().pop(
                    0,
                    &StashApplyOptions::default(),
                    &mut grit_lib::progress::NullProgress,
                    &grit_lib::progress::NeverCancel
                ),
                Err(grit_lib::error::Error::StashWouldOverwriteLocalChanges { .. })
            ));
            let mut cmd = std::process::Command::new("git");
            grit_test_support::configure_git_env(&mut cmd);
            assert!(!cmd
                .current_dir(expected.path())
                .args(["stash", "pop"])
                .output()
                .unwrap()
                .status
                .success());
            assert_eq!(
                std::fs::read(root.path().join(&path)).unwrap(),
                b"precious local\n"
            );
            assert_eq!(
                std::fs::read(root.path().join(".git/index")).unwrap(),
                before_index
            );
            assert_state_matches_git(root.path(), expected.path(), &path);
        }
    }

    fn apply_case(pop: bool, delete: bool, moved: bool, restore_index: bool, conflict: bool) {
        for path in paths() {
            let root = tempfile::tempdir().unwrap();
            setup(root.path(), &path);
            if delete {
                std::fs::remove_file(root.path().join(&path)).unwrap();
            } else {
                std::fs::write(root.path().join(&path), b"stash\n").unwrap();
            }
            if restore_index {
                git_cmd(root.path(), &["add", "-A"]);
                if !delete {
                    std::fs::write(root.path().join(&path), b"stash plus unstaged\n").unwrap();
                }
            }
            git_cmd(root.path(), &["stash", "push", "-qm", "edit"]);
            if moved {
                std::fs::write(root.path().join("head-move"), b"new head\n").unwrap();
                if conflict {
                    std::fs::write(root.path().join(&path), b"upstream\n").unwrap();
                }
                git_cmd(root.path(), &["add", "."]);
                git_cmd(root.path(), &["commit", "-qm", "advance"]);
            }
            std::fs::write(root.path().join("untouched"), b"local\n").unwrap();
            let expected = tempfile::tempdir().unwrap();
            copy_repo(root.path(), expected.path());
            let repo = open(root.path());
            let options = StashApplyOptions {
                restore_index,
                ..Default::default()
            };
            let outcome = if pop {
                repo.stash().pop(
                    0,
                    &options,
                    &mut grit_lib::progress::NullProgress,
                    &grit_lib::progress::NeverCancel,
                )
            } else {
                repo.stash().apply(
                    0,
                    &options,
                    &mut grit_lib::progress::NullProgress,
                    &grit_lib::progress::NeverCancel,
                )
            }
            .unwrap();
            assert_eq!(outcome.has_conflicts(), conflict, "{path:?}");
            if conflict {
                assert_eq!(outcome.conflicts.len(), 1);
                assert_eq!(
                    grit_lib::repo_relpath::storage_bytes(&outcome.conflicts[0].path).as_ref(),
                    path.as_os_str().as_bytes()
                );
            }
            let mut cmd = std::process::Command::new("git");
            grit_test_support::configure_git_env(&mut cmd);
            cmd.current_dir(expected.path())
                .args(["stash", if pop { "pop" } else { "apply" }]);
            if restore_index {
                cmd.arg("--index");
            }
            let output = cmd.output().unwrap();
            assert_eq!(
                output.status.success(),
                !conflict,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_state_matches_git(root.path(), expected.path(), &path);
        }
    }

    #[test]
    fn clean_apply_and_pop_preserve_raw_path_identity() {
        apply_case(false, false, false, false, false);
        apply_case(true, false, false, false, false);
    }

    #[test]
    fn pop_deletes_raw_paths() {
        apply_case(true, true, false, false, false);
    }

    #[test]
    fn pop_raw_paths_after_unrelated_head_move() {
        apply_case(true, false, true, false, false);
    }

    #[test]
    fn pop_restores_raw_path_index() {
        apply_case(true, false, false, true, false);
        apply_case(true, true, false, true, false);
        apply_case(true, false, true, true, false);
    }

    #[test]
    fn conflicted_pop_keeps_raw_path_stash_and_index_stages() {
        apply_case(true, false, true, false, true);
    }

    #[test]
    fn pop_prefix_paths_uses_original_name_for_attributes() {
        for path in paths().into_iter().take(2) {
            let root = tempfile::tempdir().unwrap();
            setup(root.path(), &path);
            std::fs::write(root.path().join(".gitattributes"), b"*.txt text eol=crlf\n").unwrap();
            git_cmd(root.path(), &["add", "."]);
            git_cmd(root.path(), &["commit", "-qm", "attributes"]);
            std::fs::write(root.path().join(&path), b"stashed\n").unwrap();
            git_cmd(root.path(), &["stash", "push", "-qm", "edit"]);
            std::fs::write(root.path().join("untouched"), b"local\n").unwrap();
            let expected = tempfile::tempdir().unwrap();
            copy_repo(root.path(), expected.path());
            let outcome = open(root.path())
                .stash()
                .pop(
                    0,
                    &StashApplyOptions::default(),
                    &mut grit_lib::progress::NullProgress,
                    &grit_lib::progress::NeverCancel,
                )
                .unwrap();
            assert!(!outcome.has_conflicts());
            git_cmd(expected.path(), &["stash", "pop"]);
            assert_eq!(
                std::fs::read(root.path().join(&path)).unwrap(),
                b"stashed\r\n"
            );
            assert_state_matches_git(root.path(), expected.path(), &path);
        }
    }

    #[test]
    fn pop_index_restores_renames_from_raw_paths() {
        for path in paths() {
            let root = tempfile::tempdir().unwrap();
            setup(root.path(), &path);
            std::fs::rename(root.path().join(&path), root.path().join("renamed.txt")).unwrap();
            git_cmd(root.path(), &["add", "-A"]);
            git_cmd(root.path(), &["stash", "push", "-qm", "rename"]);
            std::fs::write(root.path().join("untouched"), b"local\n").unwrap();
            let expected = tempfile::tempdir().unwrap();
            copy_repo(root.path(), expected.path());
            let outcome = open(root.path())
                .stash()
                .pop(
                    0,
                    &StashApplyOptions {
                        restore_index: true,
                        ..Default::default()
                    },
                    &mut grit_lib::progress::NullProgress,
                    &grit_lib::progress::NeverCancel,
                )
                .unwrap();
            assert!(!outcome.has_conflicts());
            git_cmd(expected.path(), &["stash", "pop", "--index"]);
            assert_eq!(
                std::fs::read(root.path().join("renamed.txt")).unwrap(),
                b"base\n"
            );
            assert_state_matches_git(root.path(), expected.path(), &path);
        }
    }
}
