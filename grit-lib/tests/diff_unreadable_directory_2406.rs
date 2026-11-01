//! Unreadable directories must warn and must not abort diff or stash (issue #2406).

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::diagnostics::{CollectingDiagnostics, DiagnosticsHandle, Warning};
    use grit_lib::diff::{DiffOptions, DiffStatus, DiffTarget};
    use grit_lib::progress::{NeverCancel, NullProgress};
    use grit_lib::repo::Repository;
    use grit_lib::stash::StashPushOptions;
    use grit_test_support::git;

    fn set_mode(path: &Path, mode: u32) {
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).expect("set_permissions");
    }

    struct RepoFixture {
        repo: Repository,
        diagnostics: Arc<CollectingDiagnostics>,
    }

    fn open_repo(root: &Path) -> RepoFixture {
        let diagnostics = Arc::new(CollectingDiagnostics::new());
        let mut repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open repo");
        repo.set_diagnostics(Arc::clone(&diagnostics) as DiagnosticsHandle);
        RepoFixture { repo, diagnostics }
    }

    fn init_pub_priv_base(root: &Path) {
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::create_dir(root.join("pub")).expect("mkdir pub");
        fs::create_dir(root.join("priv")).expect("mkdir priv");
        fs::write(root.join("pub/a"), "a\n").expect("write pub/a");
        fs::write(root.join("priv/b"), "b\n").expect("write priv/b");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
    }

    #[test]
    fn diff_shows_sibling_when_priv_directory_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_pub_priv_base(root);

        fs::write(root.join("pub/a"), "a\nmod\n").expect("edit pub/a");
        fs::write(root.join("priv/new.txt"), "new\n").expect("write priv/new.txt");
        set_mode(&root.join("priv"), 0o000);
        if fs::read_dir(root.join("priv")).is_ok() {
            set_mode(&root.join("priv"), 0o755);
            return;
        }

        let fx = open_repo(root);
        let files = fx
            .repo
            .diff()
            .files(&DiffTarget::WorktreeVsHead, &[], &DiffOptions::default())
            .expect("diff must succeed");
        assert!(
            files.iter().any(|f| f.path == "pub/a"),
            "pub/a diff expected: {:?}",
            files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert!(
            !files.iter().any(|p| p.path.starts_with("priv/")),
            "priv paths must not appear as modified/deleted: {:?}",
            files
        );

        let warnings = fx.diagnostics.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                Warning::WorktreeAccessFailed { path, .. }
                    if path.contains("priv/b") || path.starts_with("priv/")
            )),
            "expected warning for priv/, got {warnings:?}"
        );

        let git_diff = git(root, &["diff", "pub/a"]);
        assert!(
            git_diff.contains("mod"),
            "system git should show pub/a hunk: {git_diff}"
        );

        set_mode(&root.join("priv"), 0o755);
    }

    #[test]
    fn diff_with_pathspec_skips_unreadable_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_pub_priv_base(root);

        fs::write(root.join("pub/a"), "a\nmod\n").expect("edit pub/a");
        set_mode(&root.join("priv"), 0o000);
        if fs::read_dir(root.join("priv")).is_ok() {
            set_mode(&root.join("priv"), 0o755);
            return;
        }

        let fx = open_repo(root);
        let files = fx
            .repo
            .diff()
            .files(
                &DiffTarget::WorktreeVsHead,
                &["pub".into()],
                &DiffOptions::default(),
            )
            .expect("pathspec diff must succeed");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "pub/a");
        assert!(
            fx.diagnostics.warnings().is_empty(),
            "pathspec limited to pub must not warn about priv"
        );

        set_mode(&root.join("priv"), 0o755);
    }

    #[test]
    fn stash_saves_readable_change_when_priv_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_pub_priv_base(root);

        fs::write(root.join("pub/a"), "a\nstash\n").expect("edit pub/a");
        set_mode(&root.join("priv"), 0o000);
        if fs::read_dir(root.join("priv")).is_ok() {
            set_mode(&root.join("priv"), 0o755);
            return;
        }

        let fx = open_repo(root);
        let opts = StashPushOptions {
            message: None,
            include_untracked: false,
            identity: "Test <t@example.com> 1000000000 +0000"
                .parse()
                .expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: grit_lib::porcelain::stash::StashMode::default(),
        };
        fx.repo
            .stash()
            .push(&opts, &mut NullProgress, &NeverCancel)
            .expect("stash must succeed");

        set_mode(&root.join("priv"), 0o755);
        git(root, &["fsck"]);
        let stash_show = git(root, &["stash", "show", "-p"]);
        assert!(
            stash_show.contains("stash"),
            "stash should include pub/a change: {stash_show}"
        );
        let priv_b = fs::read_to_string(root.join("priv/b")).expect("priv/b readable again");
        assert_eq!(priv_b, "b\n");
    }

    #[test]
    fn pathspec_shows_staged_deletion_absent_from_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("old.txt"), "x\n").expect("write old.txt");
        fs::create_dir(root.join("dir")).expect("mkdir dir");
        fs::write(root.join("dir/nested.txt"), "n\n").expect("write dir/nested.txt");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["rm", "old.txt", "dir/nested.txt"]);

        let fx = open_repo(root);
        let exact = fx
            .repo
            .diff()
            .entries(
                &DiffTarget::WorktreeVsHead,
                &["old.txt".into()],
                &DiffOptions::default(),
            )
            .expect("diff old.txt");
        assert!(
            exact
                .iter()
                .any(|e| e.path() == "old.txt" && e.status == DiffStatus::Deleted),
            "expected staged deletion of old.txt, got {exact:?}"
        );

        let dir_spec = fx
            .repo
            .diff()
            .entries(
                &DiffTarget::WorktreeVsHead,
                &["dir/".into()],
                &DiffOptions::default(),
            )
            .expect("diff dir/");
        assert!(
            dir_spec
                .iter()
                .any(|e| { e.path() == "dir/nested.txt" && e.status == DiffStatus::Deleted }),
            "expected staged deletion under dir/, got {dir_spec:?}"
        );

        let git_diff = git(root, &["diff", "HEAD", "--", "old.txt"]);
        assert!(
            git_diff.contains("deleted file"),
            "git should show deletion: {git_diff}"
        );
    }

    #[test]
    fn attr_pathspec_shows_staged_deletion_from_head_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::write(root.join(".gitattributes"), "*.txt foo\n").expect("write attributes");
        fs::write(root.join("old.txt"), "x\n").expect("write old.txt");
        fs::write(root.join("other.c"), "c\n").expect("write other.c");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["rm", "old.txt"]);

        let fx = open_repo(root);
        let entries = fx
            .repo
            .diff()
            .entries(
                &DiffTarget::WorktreeVsHead,
                &[":(attr:foo)".into()],
                &DiffOptions::default(),
            )
            .expect("attr pathspec diff");
        assert!(
            entries
                .iter()
                .any(|e| { e.path() == "old.txt" && e.status == DiffStatus::Deleted }),
            "expected old.txt deletion for :(attr:foo), got {entries:?}"
        );

        let git_diff = git(root, &["diff", "HEAD", "--", ":(attr:foo)"]);
        assert!(
            git_diff.contains("old.txt"),
            "git should show old.txt under attr pathspec: {git_diff}"
        );
    }
}
