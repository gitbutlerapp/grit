//! Round-trip commit scope scenarios: grit vs system `git commit --only`.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::ResolvedPathspecs;

fn ident() -> String {
    "Scope Compat <scope@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: Some(id.parse().expect("signature")),
        committer: Some(id.parse().expect("signature")),
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        ..Default::default()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Scope Compat")
        .env("GIT_AUTHOR_EMAIL", "scope@example.com")
        .env("GIT_COMMITTER_NAME", "Scope Compat")
        .env("GIT_COMMITTER_EMAIL", "scope@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn ls_files(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["ls-files", "-s"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("ls-files");
    assert!(out.status.success());
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

fn blob_at_head(dir: &Path, path: &str) -> String {
    let spec = format!("HEAD:{path}");
    let out = Command::new("git")
        .current_dir(dir)
        .args(["show", &spec])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git show");
    assert!(
        out.status.success(),
        "git show {spec}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn tree_oid(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD^{tree}"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse tree");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn fsck(dir: &Path) {
    git(dir, &["fsck", "--strict"]);
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn init_pair() -> (tempfile::TempDir, tempfile::TempDir) {
    let grit_dir = tempfile::tempdir().expect("grit dir");
    let git_dir = tempfile::tempdir().expect("git dir");
    git(grit_dir.path(), &["init", "-q", "-b", "main", "."]);
    git(git_dir.path(), &["init", "-q", "-b", "main", "."]);
    (grit_dir, git_dir)
}

fn seed_initial(grit_root: &Path, git_root: &Path) {
    for root in [grit_root, git_root] {
        std::fs::write(root.join("file"), b"1\n").expect("file");
        std::fs::write(root.join("baz"), b"1\n").expect("baz");
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "initial"]);
    }
}

#[test]
fn paths_commit_matches_git_only() {
    let (grit_tmp, git_tmp) = init_pair();
    let grit_root = grit_tmp.path();
    let git_root = git_tmp.path();
    seed_initial(grit_root, git_root);

    for root in [grit_root, git_root] {
        std::fs::write(root.join("file"), b"2\n").expect("file");
        std::fs::write(root.join("baz"), b"2\n").expect("baz");
        git(root, &["add", "baz"]);
    }

    let repo = open_grit(grit_root);
    let paths = ResolvedPathspecs {
        specs: vec!["file".into()],
        sources: vec!["file".into()],
        scoped: true,
    };
    commit_with_scope(
        &repo,
        &commit_req("file only"),
        CommitScope::Paths(paths),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit paths commit");
    git(
        git_root,
        &["commit", "-m", "file only", "--only", "--", "file"],
    );

    assert_eq!(tree_oid(grit_root), tree_oid(git_root));
    assert_eq!(ls_files(grit_root), ls_files(git_root));
    fsck(grit_root);
    fsck(git_root);
}

/// Regression for <https://github.com/gitbutlerapp/grit/issues/2006>.
#[test]
fn paths_commit_force_added_ignored_file_matches_git() {
    let (grit_tmp, git_tmp) = init_pair();
    let grit_root = grit_tmp.path();
    let git_root = git_tmp.path();

    for root in [grit_root, git_root] {
        std::fs::write(root.join(".gitignore"), "build\n").expect("gitignore");
        std::fs::create_dir_all(root.join("build")).expect("build dir");
        std::fs::write(root.join("build/x.o"), b"o\n").expect("object file");
        git(root, &["add", "-f", "build/x.o"]);
    }

    let repo = open_grit(grit_root);
    let paths = ResolvedPathspecs {
        specs: vec!["build/x.o".into()],
        sources: vec!["build/x.o".into()],
        scoped: true,
    };
    commit_with_scope(
        &repo,
        &commit_req("one"),
        CommitScope::Paths(paths.clone()),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit paths commit force-added ignored");
    git(
        git_root,
        &["commit", "-m", "one", "--only", "--", "build/x.o"],
    );

    assert_eq!(tree_oid(grit_root), tree_oid(git_root));
    fsck(grit_root);
    fsck(git_root);

    for root in [grit_root, git_root] {
        std::fs::write(root.join("build/x.o"), b"o\nmore\n").expect("modify ignored tracked");
    }

    commit_with_scope(
        &repo,
        &commit_req("two"),
        CommitScope::Paths(paths),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit paths commit modified ignored tracked");
    git(
        git_root,
        &["commit", "-m", "two", "--only", "--", "build/x.o"],
    );

    assert_eq!(tree_oid(grit_root), tree_oid(git_root));
    assert_eq!(ls_files(grit_root), ls_files(git_root));
    fsck(grit_root);
    fsck(git_root);
}

#[test]
fn paths_commit_tracked_ignored_after_initial_commit_matches_git() {
    let (grit_tmp, git_tmp) = init_pair();
    let grit_root = grit_tmp.path();
    let git_root = git_tmp.path();
    seed_initial(grit_root, git_root);

    for root in [grit_root, git_root] {
        std::fs::create_dir_all(root.join("build")).expect("build dir");
        std::fs::write(root.join("build/x.o"), b"tracked\n").expect("object file");
        git(root, &["add", "build/x.o"]);
        git(root, &["commit", "-m", "track build"]);
        std::fs::write(root.join(".gitignore"), "build\n").expect("gitignore");
    }

    for root in [grit_root, git_root] {
        std::fs::write(root.join("build/x.o"), b"tracked\nmodified\n").expect("modify");
    }

    let repo = open_grit(grit_root);
    let paths = ResolvedPathspecs {
        specs: vec!["build/x.o".into()],
        sources: vec!["build/x.o".into()],
        scoped: true,
    };
    commit_with_scope(
        &repo,
        &commit_req("path only"),
        CommitScope::Paths(paths),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit paths commit tracked ignored");
    git(
        git_root,
        &["commit", "-m", "path only", "--only", "--", "build/x.o"],
    );

    assert_eq!(tree_oid(grit_root), tree_oid(git_root));
    fsck(grit_root);
    fsck(git_root);
}

#[test]
fn paths_commit_directory_pathspec_on_ignored_tree_matches_git() {
    let (grit_tmp, git_tmp) = init_pair();
    let grit_root = grit_tmp.path();
    let git_root = git_tmp.path();

    for root in [grit_root, git_root] {
        std::fs::write(root.join(".gitignore"), "build\n").expect("gitignore");
        std::fs::create_dir_all(root.join("build")).expect("build dir");
        std::fs::write(root.join("build/x.o"), b"o\n").expect("object file");
        git(root, &["add", "-f", "build/x.o"]);
    }

    let paths = ResolvedPathspecs {
        specs: vec!["build".into()],
        sources: vec!["build".into()],
        scoped: true,
    };
    let repo = open_grit(grit_root);
    commit_with_scope(
        &repo,
        &commit_req("one"),
        CommitScope::Paths(paths),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit commit with directory pathspec on ignored tree");
    git(git_root, &["commit", "-m", "one", "--only", "--", "build"]);

    assert_eq!(tree_oid(grit_root), tree_oid(git_root));
    assert_eq!(blob_at_head(grit_root, "build/x.o"), "o\n");
    fsck(grit_root);
    fsck(git_root);
}

#[test]
fn paths_commit_exclude_pathspec_matches_git() {
    let (grit_tmp, git_tmp) = init_pair();
    let grit_root = grit_tmp.path();
    let git_root = git_tmp.path();

    for root in [grit_root, git_root] {
        std::fs::create_dir_all(root.join("build")).expect("build dir");
        std::fs::write(root.join("build/x.o"), b"x1\n").expect("x.o");
        std::fs::write(root.join("build/y.o"), b"y1\n").expect("y.o");
        git(root, &["add", "-f", "build/x.o", "build/y.o"]);
        git(root, &["commit", "-m", "initial"]);
        std::fs::write(root.join(".gitignore"), "build\n").expect("gitignore");
        std::fs::write(root.join("build/x.o"), b"x2\n").expect("modify x");
        std::fs::write(root.join("build/y.o"), b"y2\n").expect("modify y");
        git(root, &["add", "-f", "build/y.o"]);
    }

    let paths = ResolvedPathspecs {
        specs: vec![":(glob)build/*.o".into(), ":(exclude)build/y.o".into()],
        sources: vec![":(glob)build/*.o".into(), ":(exclude)build/y.o".into()],
        scoped: true,
    };

    let repo = open_grit(grit_root);
    commit_with_scope(
        &repo,
        &commit_req("scoped"),
        CommitScope::Paths(paths),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit paths commit with exclude");
    git(
        git_root,
        &[
            "commit",
            "-m",
            "scoped",
            "--only",
            "--",
            ":(glob)build/*.o",
            ":(exclude)build/y.o",
        ],
    );

    assert_eq!(tree_oid(grit_root), tree_oid(git_root));
    assert_eq!(blob_at_head(grit_root, "build/x.o"), "x2\n");
    assert_eq!(blob_at_head(git_root, "build/x.o"), "x2\n");
    assert_eq!(blob_at_head(grit_root, "build/y.o"), "y1\n");
    assert_eq!(blob_at_head(git_root, "build/y.o"), "y1\n");
    fsck(grit_root);
    fsck(git_root);
}

#[test]
fn staged_commit_matches_git_without_auto_stage() {
    let (grit_tmp, git_tmp) = init_pair();
    let grit_root = grit_tmp.path();
    let git_root = git_tmp.path();
    seed_initial(grit_root, git_root);

    for root in [grit_root, git_root] {
        std::fs::write(root.join("file"), b"2\n").expect("file");
        git(root, &["add", "file"]);
    }

    let repo = open_grit(grit_root);
    commit_with_scope(
        &repo,
        &commit_req("staged"),
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit staged");
    git(git_root, &["commit", "-m", "staged"]);

    assert_eq!(tree_oid(grit_root), tree_oid(git_root));
    assert_eq!(ls_files(grit_root), ls_files(git_root));
    fsck(grit_root);
}
