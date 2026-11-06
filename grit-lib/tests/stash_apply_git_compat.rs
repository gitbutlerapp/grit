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
            &grit_lib::progress::CancelToken::never(),
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
            &grit_lib::progress::CancelToken::never(),
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
                    &grit_lib::progress::CancelToken::never(),
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
                    &grit_lib::progress::CancelToken::never()
                ),
                Err(grit_lib::stash::StashError::WouldOverwriteLocalChanges { .. })
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
                    &grit_lib::progress::CancelToken::never(),
                )
            } else {
                repo.stash().apply(
                    0,
                    &options,
                    &mut grit_lib::progress::NullProgress,
                    &grit_lib::progress::CancelToken::never(),
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
                    &grit_lib::progress::CancelToken::never(),
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
                    &grit_lib::progress::CancelToken::never(),
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

    #[test]
    fn push_untracked_literal_pathhex_apply_restores_exact_filename() {
        use grit_lib::stash::{StashApplyOptions, StashMode, StashPushOptions};

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        std::fs::write(root.path().join("tracked.txt"), b"ok\n").unwrap();
        git_cmd(root.path(), &["add", "tracked.txt"]);
        git_cmd(root.path(), &["commit", "-qm", "base"]);

        let literal = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"grit+pathhex:6162"));
        std::fs::write(root.path().join(&literal), b"secret\n").unwrap();

        let identity: grit_lib::ident::Signature =
            "Test <t@example.com> 1000000000 +0000".parse().unwrap();
        open(root.path())
            .stash()
            .push(
                &StashPushOptions {
                    message: Some("untracked literal".into()),
                    include_untracked: true,
                    identity,
                    selection: None,
                    pathspecs: Vec::new(),
                    dry_run: false,
                    mode: StashMode::default(),
                },
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .unwrap()
            .expect("stash changes");

        assert!(
            !root.path().join(&literal).exists(),
            "push -u must remove the untracked literal path from the worktree"
        );

        open(root.path())
            .stash()
            .apply(
                0,
                &StashApplyOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .unwrap();

        assert_eq!(
            std::fs::read(root.path().join(&literal)).unwrap(),
            b"secret\n"
        );
        let pathhex_entries: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("grit+pathhex:"))
            .collect();
        assert_eq!(
            pathhex_entries.len(),
            1,
            "must not duplicate literal pathhex name as an encoded path"
        );
        git_cmd(root.path(), &["fsck", "--strict"]);
    }

    #[test]
    fn push_untracked_non_utf8_apply_restores_raw_bytes() {
        use grit_lib::stash::{StashApplyOptions, StashMode, StashPushOptions};

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        std::fs::write(root.path().join("tracked.txt"), b"ok\n").unwrap();
        git_cmd(root.path(), &["add", "tracked.txt"]);
        git_cmd(root.path(), &["commit", "-qm", "base"]);

        let raw = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"nested/\xff.txt"));
        std::fs::create_dir_all(root.path().join(raw.parent().unwrap())).unwrap();
        std::fs::write(root.path().join(&raw), b"raw-bytes\n").unwrap();

        let identity: grit_lib::ident::Signature =
            "Test <t@example.com> 1000000000 +0000".parse().unwrap();
        open(root.path())
            .stash()
            .push(
                &StashPushOptions {
                    message: Some("untracked non-utf8".into()),
                    include_untracked: true,
                    identity,
                    selection: None,
                    pathspecs: Vec::new(),
                    dry_run: false,
                    mode: StashMode::default(),
                },
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .unwrap()
            .expect("stash changes");

        open(root.path())
            .stash()
            .apply(
                0,
                &StashApplyOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .unwrap();

        assert_eq!(
            std::fs::read(root.path().join(&raw)).unwrap(),
            b"raw-bytes\n"
        );
        git_cmd(root.path(), &["fsck", "--strict"]);
    }
}

#[cfg(unix)]
mod grit_stash_push_non_utf8 {
    use super::*;
    use grit_lib::stash::{StashApplyOptions, StashPushOptions};
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    fn path_bytes(bytes: &[u8]) -> std::path::PathBuf {
        PathBuf::from(OsStr::from_bytes(bytes))
    }

    fn stash_push_opts(include_untracked: bool) -> StashPushOptions {
        StashPushOptions {
            message: None,
            include_untracked,
            identity: identity(),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: Default::default(),
        }
    }

    fn identity() -> grit_lib::ident::Signature {
        grit_lib::ident::Signature::from_header_lossy("Test <t@example.com> 1000000000 +0000")
    }

    fn open(root: &Path) -> Repository {
        Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open")
    }

    fn tree_paths(repo: &Path, rev: &str) -> Vec<Vec<u8>> {
        let mut cmd = std::process::Command::new("git");
        grit_test_support::configure_git_env(&mut cmd);
        let out = cmd
            .current_dir(repo)
            .args(["ls-tree", "-r", "--name-only", "-z", rev])
            .output()
            .expect("ls-tree");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| p.to_vec())
            .collect()
    }

    fn assert_no_pathhex_tree_names(paths: &[Vec<u8>]) {
        for p in paths {
            let s = String::from_utf8_lossy(p);
            assert!(
                !s.starts_with("grit+pathhex:"),
                "stash tree must not contain encoded path names, got {s:?}"
            );
        }
    }

    #[test]
    fn push_tracked_non_utf8_round_trips_with_git() {
        let root = tempfile::tempdir().expect("tempdir");
        init_repo(root.path());
        let non_utf8 = path_bytes(b"base\xff.txt");
        std::fs::write(root.path().join(&non_utf8), b"a").expect("write");
        std::fs::write(root.path().join("plain.txt"), b"x").expect("write");
        git_cmd(root.path(), &["add", "-A"]);
        git_cmd(root.path(), &["commit", "-qm", "one"]);
        std::fs::write(root.path().join(&non_utf8), b"ab").expect("modify");
        std::fs::write(root.path().join("plain.txt"), b"xy\n").expect("modify");

        let repo = open(root.path());
        repo.stash()
            .push(
                &stash_push_opts(false),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("push")
            .expect("stash oid");

        assert!(
            git_cmd(root.path(), &["status", "--porcelain"])
                .trim()
                .is_empty(),
            "worktree must be clean after grit stash"
        );

        let w_tree_paths = tree_paths(root.path(), "stash@{0}");
        assert_eq!(w_tree_paths.len(), 2);
        assert_no_pathhex_tree_names(&w_tree_paths);
        assert!(w_tree_paths
            .iter()
            .any(|p| p == non_utf8.as_os_str().as_bytes()));

        let show = git_cmd(root.path(), &["stash", "show", "--name-only"]);
        assert!(
            !show.contains("grit+pathhex:"),
            "git stash show must not list pathhex names:\n{show}"
        );
        git_cmd(root.path(), &["fsck", "--strict"]);

        repo.stash()
            .pop(
                0,
                &StashApplyOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("pop");

        let porcelain = git_cmd(root.path(), &["status", "--porcelain"]);
        assert!(porcelain.contains("plain.txt"));
        assert!(
            !porcelain.contains("grit+pathhex:"),
            "must not create stray pathhex file:\n{porcelain}"
        );
        assert_eq!(std::fs::read(root.path().join(&non_utf8)).unwrap(), b"ab");
        assert_eq!(
            std::fs::read(root.path().join("plain.txt")).unwrap(),
            b"xy\n"
        );
        git_cmd(root.path(), &["fsck", "--strict"]);
    }

    #[test]
    fn push_untracked_non_utf8_removes_and_restores() {
        let root = tempfile::tempdir().expect("tempdir");
        init_repo(root.path());
        let non_utf8 = path_bytes(b"un\xff");
        std::fs::write(root.path().join("tracked.txt"), b"a").expect("write");
        git_cmd(root.path(), &["add", "tracked.txt"]);
        git_cmd(root.path(), &["commit", "-qm", "one"]);
        std::fs::write(root.path().join(&non_utf8), b"u").expect("untracked");

        let repo = open(root.path());
        repo.stash()
            .push(
                &stash_push_opts(true),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("push")
            .expect("stash");

        assert!(!root.path().join(&non_utf8).exists());
        assert!(git_cmd(root.path(), &["status", "--porcelain"])
            .trim()
            .is_empty());

        let ut_commit = git_cmd(root.path(), &["rev-parse", "stash@{0}^3"])
            .trim()
            .to_owned();
        let ut_paths = tree_paths(root.path(), &format!("{ut_commit}"));
        assert_eq!(ut_paths.len(), 1);
        assert_eq!(ut_paths[0], non_utf8.as_os_str().as_bytes());

        repo.stash()
            .pop(
                0,
                &StashApplyOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("pop");

        assert_eq!(std::fs::read(root.path().join(&non_utf8)).unwrap(), b"u");
        git_cmd(root.path(), &["fsck", "--strict"]);
    }

    #[test]
    fn push_literal_grit_pathhex_filename_stays_distinct() {
        let root = tempfile::tempdir().expect("tempdir");
        init_repo(root.path());
        let literal = path_bytes(b"grit+pathhex:plain.txt");
        std::fs::write(root.path().join(&literal), b"a").expect("write");
        git_cmd(root.path(), &["add", "-A"]);
        git_cmd(root.path(), &["commit", "-qm", "one"]);
        std::fs::write(root.path().join(&literal), b"ab").expect("modify");

        let repo = open(root.path());
        repo.stash()
            .push(
                &stash_push_opts(false),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("push")
            .expect("stash");

        let w_paths = tree_paths(root.path(), "stash@{0}");
        assert_eq!(w_paths.len(), 1);
        assert_eq!(w_paths[0], literal.as_os_str().as_bytes());

        repo.stash()
            .pop(
                0,
                &StashApplyOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("pop");
        assert_eq!(std::fs::read(root.path().join(&literal)).unwrap(), b"ab");
    }

    #[test]
    fn pop_rejects_non_utf8_untracked_collision_before_tracked_apply() {
        let root = tempfile::tempdir().expect("tempdir");
        init_repo(root.path());
        std::fs::write(root.path().join("f"), b"a").expect("write f");
        git_cmd(root.path(), &["add", "f"]);
        git_cmd(root.path(), &["commit", "-qm", "one"]);

        std::fs::write(root.path().join("f"), b"b").expect("modify tracked");
        let child = path_bytes(b"dir\xff/child");
        std::fs::create_dir_all(root.path().join(&child).parent().unwrap()).expect("mkdir");
        std::fs::write(root.path().join(&child), b"child\n").expect("untracked");

        let repo = open(root.path());
        repo.stash()
            .push(
                &stash_push_opts(true),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("push")
            .expect("stash");

        let blocker = path_bytes(b"dir\xff");
        std::fs::write(root.path().join(&blocker), b"blocker\n").expect("blocker file");

        let err = repo
            .stash()
            .pop(
                0,
                &StashApplyOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect_err("must reject before mutating tracked files");
        assert!(matches!(
            err,
            grit_lib::stash::StashError::CouldNotRestoreUntracked { .. }
        ));

        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"a");
        assert_eq!(
            std::fs::read(root.path().join(&blocker)).unwrap(),
            b"blocker\n"
        );
        assert!(
            !git_cmd(root.path(), &["rev-parse", "refs/stash"])
                .trim()
                .is_empty(),
            "stash entry must remain after failed pop"
        );
    }

    #[test]
    fn git_created_stash_of_non_utf8_still_pops_in_grit() {
        let root = tempfile::tempdir().expect("tempdir");
        init_repo(root.path());
        let non_utf8 = path_bytes(b"base\xff.txt");
        std::fs::write(root.path().join(&non_utf8), b"a").expect("write");
        git_cmd(root.path(), &["add", "-A"]);
        git_cmd(root.path(), &["commit", "-qm", "one"]);
        std::fs::write(root.path().join(&non_utf8), b"ab").expect("modify");
        git_cmd(root.path(), &["stash", "push", "-qm", "git"]);

        let repo = open(root.path());
        repo.stash()
            .pop(
                0,
                &StashApplyOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect("grit pop of git stash");
        assert_eq!(std::fs::read(root.path().join(&non_utf8)).unwrap(), b"ab");
    }
}
