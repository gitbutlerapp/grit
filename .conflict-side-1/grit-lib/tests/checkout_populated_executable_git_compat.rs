//! Fresh checkout of executable blobs (clone / empty index) must match system `git` file modes.

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::objects::ObjectId;
    use grit_lib::porcelain::checkout::checkout_between_trees;
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    fn tree_of_head(repo: &std::path::Path) -> ObjectId {
        let hex = git(repo, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
        ObjectId::from_hex(&hex).expect("tree oid")
    }

    fn mode_bits(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    static UMASK_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_umask<R>(mask: libc::mode_t, f: impl FnOnce() -> R) -> R {
        let _guard = UMASK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = unsafe { libc::umask(mask) };
        let out = f();
        unsafe {
            libc::umask(old);
        }
        out
    }

    fn fresh_executable_checkout_oracle(umask: libc::mode_t, expected_mode: u32) {
        with_umask(umask, || {
            let tmp = tempfile::tempdir().expect("tempdir");
            let source = tmp.path().join("source");
            std::fs::create_dir(&source).expect("source dir");

            git(&source, &["init", "-q", "-b", "main", "."]);
            git(&source, &["config", "user.email", "t@example.com"]);
            git(&source, &["config", "user.name", "Test"]);
            std::fs::write(source.join("run.sh"), b"#!/bin/sh\n").expect("write script");
            let mut perms = std::fs::metadata(source.join("run.sh"))
                .expect("meta")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(source.join("run.sh"), perms).expect("chmod");
            git(&source, &["add", "."]);
            git(&source, &["commit", "-qm", "add script"]);
            let tree = tree_of_head(&source);

            let git_dest = tmp.path().join("git-clone");
            git(
                tmp.path(),
                &[
                    "clone",
                    "-q",
                    source.to_str().expect("utf8"),
                    git_dest.to_str().expect("utf8"),
                ],
            );
            let git_mode = mode_bits(&git_dest.join("run.sh"));
            assert_eq!(
                git_mode, expected_mode,
                "git clone mode should be {expected_mode:#o}"
            );

            let grit_dest = tmp.path().join("grit-checkout");
            std::fs::create_dir(&grit_dest).expect("grit dest");
            std::process::Command::new("git")
                .args([
                    "clone",
                    "-q",
                    "--no-checkout",
                    source.to_str().expect("utf8"),
                    grit_dest.to_str().expect("utf8"),
                ])
                .status()
                .expect("git clone no-checkout");
            let grit_repo = Repository::open(
                &grit_dest.join(".git"),
                &RepositoryOptions::empty().with_work_tree(&grit_dest),
            )
            .expect("open");
            checkout_between_trees(&grit_repo, None, &tree).expect("fresh checkout");
            let grit_mode = mode_bits(&grit_dest.join("run.sh"));
            assert_eq!(
                grit_mode, git_mode,
                "grit fresh executable checkout must match git (expected {git_mode:#o}, got {grit_mode:#o})"
            );
        });
    }

    #[test]
    fn fresh_executable_checkout_matches_git_clone_under_umask_022() {
        fresh_executable_checkout_oracle(0o022, 0o755);
    }

    #[test]
    fn fresh_executable_checkout_matches_git_clone_under_umask_077() {
        fresh_executable_checkout_oracle(0o077, 0o700);
    }

    #[test]
    fn fresh_executable_checkout_matches_git_clone_under_umask_011() {
        fresh_executable_checkout_oracle(0o011, 0o766);
    }

    #[test]
    fn fresh_executable_checkout_matches_git_clone_under_umask_033() {
        fresh_executable_checkout_oracle(0o033, 0o744);
    }
}
