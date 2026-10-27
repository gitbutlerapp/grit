//! A tracked file that cannot be opened for content comparison is reported as modified, not a
//! failed scan (issue #2107). Mirrors a file held exclusively by another process on Windows.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::diff::DiffStatus;
    use grit_lib::error::{Error, OverwriteReason, WorktreePathOperation};
    use grit_lib::index::Index;
    use grit_lib::objects::ObjectId;
    use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
    use grit_lib::porcelain::status::{status, StatusOptions};
    use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
    use grit_lib::progress::{NeverCancel, NullProgress};
    use grit_lib::repo::Repository;
    use grit_test_support::{forced_parallel_policy, git};

    /// Above the parallel stat/hash threshold of the status scan.
    const PARALLEL_FILE_COUNT: usize = 1200;

    struct Fixture {
        dir: tempfile::TempDir,
        other_oid: ObjectId,
    }

    impl Fixture {
        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn open(&self) -> Repository {
            Repository::open(
                &self.root().join(".git"),
                &RepositoryOptions::empty().with_work_tree(self.root()),
            )
            .expect("open repo")
        }

        fn locked_index_entry(&self) -> (u32, u32, u32) {
            let index = Index::load(&self.root().join(".git/index")).expect("load index");
            let entry = index.get(b"a.txt", 0).expect("a.txt in index");
            (entry.mtime_sec, entry.mtime_nsec, entry.size)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            set_mode(&self.dir.path().join("a.txt"), 0o644);
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        let Ok(meta) = fs::metadata(path) else {
            return;
        };
        let mut perms = meta.permissions();
        perms.set_mode(mode);
        let _ = fs::set_permissions(path, perms);
    }

    /// Two tracked files (plus `extra` siblings), a branch `other` that changes `a.txt`, and
    /// `a.txt` left with a stale index stat and no read permission.
    fn setup(extra: usize) -> Option<Fixture> {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("a.txt"), "one\n").expect("write a");
        fs::write(root.join("b.txt"), "two\n").expect("write b");
        if extra > 0 {
            fs::create_dir(root.join("many")).expect("mkdir many");
            for i in 0..extra {
                fs::write(root.join(format!("many/f{i:05}.txt")), format!("{i}\n"))
                    .expect("write extra");
            }
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "c1"]);
        git(root, &["switch", "-q", "-c", "other"]);
        fs::write(root.join("a.txt"), "two\n").expect("write a on other");
        git(root, &["commit", "-qam", "c2"]);
        let other_oid =
            ObjectId::from_hex(git(root, &["rev-parse", "other"]).trim()).expect("other oid");
        git(root, &["switch", "-q", "main"]);

        let stale = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(root.join("a.txt"))
            .expect("open a for mtime")
            .set_modified(stale)
            .expect("set mtime");
        set_mode(&root.join("a.txt"), 0o000);
        if fs::File::open(root.join("a.txt")).is_ok() {
            // Running with privileges that ignore file permissions; nothing to exercise.
            set_mode(&root.join("a.txt"), 0o644);
            return None;
        }
        Some(Fixture { dir, other_oid })
    }

    fn assert_only_locked_path_modified(fx: &Fixture, repo: &Repository, threads: usize) {
        let before = fx.locked_index_entry();
        let opts = StatusOptions {
            stat_parallel_threads: Some(threads),
            ..StatusOptions::default()
        };
        let model = status(repo, &opts, &mut NullProgress, &NeverCancel)
            .expect("status succeeds with an unreadable tracked file");

        assert!(model.staged.is_empty(), "staged: {:?}", model.staged);
        assert_eq!(model.unstaged.len(), 1, "unstaged: {:?}", model.unstaged);
        let entry = &model.unstaged[0];
        assert_eq!(entry.path(), "a.txt");
        assert_eq!(entry.status, DiffStatus::Modified);
        assert!(
            model.untracked.is_empty(),
            "untracked: {:?}",
            model.untracked
        );
        assert_eq!(
            fx.locked_index_entry(),
            before,
            "unreadable path must not adopt the new stat"
        );

        let git_status = git(fx.root(), &["status", "--porcelain"]);
        assert_eq!(git_status, " M a.txt\n", "system git agrees");
    }

    #[test]
    fn status_reports_unreadable_tracked_file_as_modified_serial() {
        let Some(fx) = setup(0) else { return };
        let repo = fx.open();
        assert_only_locked_path_modified(&fx, &repo, 1);
    }

    #[test]
    fn status_reports_unreadable_tracked_file_as_modified_parallel() {
        let Some(fx) = setup(PARALLEL_FILE_COUNT) else {
            return;
        };
        let repo = fx.open().with_thread_policy(forced_parallel_policy());
        assert_only_locked_path_modified(&fx, &repo, 4);
    }

    #[test]
    fn later_edit_after_unlock_is_still_reported() {
        let Some(fx) = setup(0) else { return };
        let repo = fx.open();
        status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status while locked");
        set_mode(&fx.root().join("a.txt"), 0o644);
        fs::write(fx.root().join("a.txt"), "ONE\n").expect("edit a");
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status after unlock");
        let paths: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
        assert_eq!(paths, ["a.txt"]);

        fs::write(fx.root().join("a.txt"), "one\n").expect("restore a");
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status after restore");
        assert!(model.unstaged.is_empty(), "unstaged: {:?}", model.unstaged);
    }

    #[test]
    fn switch_refuses_to_overwrite_unreadable_path() {
        let Some(fx) = setup(0) else { return };
        let repo = fx.open();
        let head_before = git(fx.root(), &["rev-parse", "HEAD"]);
        let err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "other".into(),
                    oid: fx.other_oid,
                },
                dry_run: false,
                hooks: grit_lib::PostOperationHooks::Skip,
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect_err("switch must refuse to overwrite the unreadable path");
        match err {
            Error::WouldOverwrite {
                reason: OverwriteReason::LocalChanges,
                paths,
                ..
            } => assert_eq!(paths, ["a.txt"]),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(git(fx.root(), &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            git(fx.root(), &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/main"
        );
    }

    fn stage_path(repo: &Repository, path: &str) -> grit_lib::error::Result<()> {
        let opts = StageOptions {
            pathspecs: vec![path.to_owned()],
            pathspec_sources: vec![path.to_owned()],
            mode: StageMode::All,
            ..StageOptions::default()
        };
        stage(repo, &opts, &mut NullProgress).map(|_| ())
    }

    #[test]
    fn staging_sibling_succeeds_and_staging_unreadable_path_fails() {
        let Some(fx) = setup(0) else { return };
        let repo = fx.open();
        fs::write(fx.root().join("b.txt"), "TWO\n").expect("edit b");
        stage_path(&repo, "b.txt").expect("stage sibling of unreadable file");
        let staged = git(fx.root(), &["diff", "--cached", "--name-only"]);
        assert_eq!(staged, "b.txt\n");

        match stage_path(&repo, "a.txt") {
            Err(Error::WorktreePathIo {
                path,
                operation: WorktreePathOperation::Read,
                ..
            }) => assert_eq!(path, "a.txt"),
            other => panic!("staging an unreadable file must fail, got {other:?}"),
        }
    }

    #[test]
    fn patch_read_of_unreadable_path_still_errors() {
        let Some(fx) = setup(0) else { return };
        let repo = fx.open();
        let config = repo.config().expect("config");
        let err = grit_lib::diff::read_worktree_side_bytes_for_diff(
            repo.odb(),
            fx.root(),
            "a.txt",
            0o100644,
            &config,
            None,
            None,
        )
        .expect_err("patch content of an unreadable file cannot be read");
        assert!(
            matches!(
                err,
                Error::WorktreePathIo {
                    operation: WorktreePathOperation::Read,
                    ..
                }
            ),
            "unexpected error: {err:?}"
        );
    }
}
