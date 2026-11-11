//! Checkout delete rollback must preserve symlinks when the spool crosses devices (#1968).

use grit_lib::progress::{CancelToken, NullProgress};

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::Path;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::error::{Error, WorktreePathOperation};
    use grit_lib::objects::ObjectId;
    use grit_lib::porcelain::switch::{SwitchMode, SwitchOptions};
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    #[test]
    fn switch_rollback_preserves_symlink_when_spool_crosses_devices() {
        if !Path::new("/dev/shm").is_dir() {
            return;
        }

        let tmp = tempfile::Builder::new()
            .prefix("grit-1968-")
            .tempdir_in("/tmp")
            .expect("repo under /tmp");
        git(tmp.path(), &["init", "-q", "-b", "main", "."]);
        git(tmp.path(), &["config", "user.email", "t@example.com"]);
        git(tmp.path(), &["config", "user.name", "Test"]);

        std::fs::write(tmp.path().join("link-target"), "target bytes\n").expect("link target");
        symlink("link-target", tmp.path().join("a-link")).expect("symlink");
        std::fs::create_dir_all(tmp.path().join("z")).expect("mkdir z");
        std::fs::write(tmp.path().join("z/blocked"), "blocked\n").expect("write blocked");
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-qm", "base"]);

        git(tmp.path(), &["switch", "-q", "-c", "del"]);
        git(tmp.path(), &["rm", "-q", "a-link", "z/blocked"]);
        git(tmp.path(), &["commit", "-qm", "del"]);
        let del_oid =
            ObjectId::from_hex(git(tmp.path(), &["rev-parse", "del"]).trim()).expect("del oid");

        git(tmp.path(), &["switch", "-q", "main"]);
        let main_head = git(tmp.path(), &["rev-parse", "HEAD"]);

        let mut perms = std::fs::metadata(tmp.path().join("z"))
            .expect("meta z")
            .permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(tmp.path().join("z"), perms).expect("chmod z");

        let repo = Repository::open(
            &tmp.path().join(".git"),
            &RepositoryOptions::empty().with_work_tree(tmp.path()),
        )
        .expect("open grit repo");

        std::env::set_var("TMPDIR", "/dev/shm");
        let err = repo
            .checkout()
            .switch(
                &SwitchOptions::new(SwitchMode::Existing {
                    branch: "del".into(),
                    oid: del_oid,
                })
                .with_hooks(grit_lib::command_runner::hooks::PostOperationHooks::Skip),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .expect_err("switch must fail when z/blocked cannot be removed");
        std::env::remove_var("TMPDIR");

        let msg = err.to_string();
        assert!(
            msg.contains("z/blocked"),
            "error must name the blocking path, got: {msg}"
        );
        let path_io = err
            .path_io()
            .unwrap_or_else(|| panic!("unexpected error: {err:?}"));
        assert_eq!(
            path_io.operation,
            grit_lib::error::PathIoOperation::Worktree(WorktreePathOperation::Remove)
        );
        assert_eq!(
            path_io.path.as_deref(),
            Some(std::path::Path::new("z/blocked"))
        );

        assert_eq!(
            git(tmp.path(), &["rev-parse", "HEAD"]),
            main_head,
            "HEAD must stay on main"
        );
        assert!(
            tmp.path()
                .join("a-link")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink(),
            "a-link must remain a symlink after rollback"
        );
        assert_eq!(
            std::fs::read_link(tmp.path().join("a-link")).unwrap(),
            std::path::PathBuf::from("link-target")
        );
        let porcelain = git(tmp.path(), &["status", "--porcelain"]);
        assert!(
            !porcelain.contains(" T a-link"),
            "symlink must not become a regular file; got:\n{porcelain}"
        );
        assert!(
            tmp.path().join("z/blocked").is_file(),
            "blocked file must remain after rollback"
        );

        let mut restore = std::fs::metadata(tmp.path().join("z"))
            .expect("meta z")
            .permissions();
        restore.set_mode(0o755);
        std::fs::set_permissions(tmp.path().join("z"), restore).expect("restore z");
    }
}
