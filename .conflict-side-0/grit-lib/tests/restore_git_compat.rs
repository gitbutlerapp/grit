//! `restore_paths` compatibility with system `git`.

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::{MODE_EXECUTABLE, MODE_SYMLINK};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn git_fsck(repo: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init"]);
    git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
    git_cmd(dir.path(), &["config", "user.name", "Test"]);
    let root = dir.path();
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    (dir, repo)
}

fn restore_opts(
    pathspecs: &[&str],
    staged: bool,
    worktree: bool,
    source: RestoreSource,
) -> RestoreOptions {
    RestoreOptions {
        pathspecs: pathspecs.iter().map(|s| (*s).to_string()).collect(),
        pathspec_sources: pathspecs.iter().map(|s| (*s).to_string()).collect(),
        source,
        staged,
        worktree,
        stage: None,
        remerge: None,
    }
}

#[test]
fn restore_worktree_from_index() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("a.txt"), "v1\n").unwrap();
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "c1"]);
    fs::write(dir.path().join("a.txt"), "dirty\n").unwrap();
    git_cmd(dir.path(), &["add", "a.txt"]);
    fs::write(dir.path().join("a.txt"), "dirty2\n").unwrap();

    restore_paths(
        &repo,
        &restore_opts(&["a.txt"], false, false, RestoreSource::Index),
    )
    .expect("restore");

    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "dirty\n"
    );
    let porcelain = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert_eq!(porcelain.trim(), "M  a.txt");
    git_fsck(dir.path());
}

#[test]
fn restore_staged_and_worktree_drops_staged_new_file() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("base.txt"), "base\n").unwrap();
    git_cmd(dir.path(), &["add", "base.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "c1"]);
    fs::write(dir.path().join("new.txt"), "new\n").unwrap();
    git_cmd(dir.path(), &["add", "new.txt"]);

    restore_paths(
        &repo,
        &restore_opts(&["new.txt"], true, true, RestoreSource::Index),
    )
    .expect("restore staged and worktree");

    assert!(!dir.path().join("new.txt").exists());
    assert!(git_cmd(dir.path(), &["ls-files", "new.txt"])
        .trim()
        .is_empty());
    assert!(git_cmd(dir.path(), &["status", "--porcelain"])
        .trim()
        .is_empty());
    git_fsck(dir.path());
}

#[test]
fn restore_staged_unstages() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("b.txt"), "base\n").unwrap();
    git_cmd(dir.path(), &["add", "b.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "c1"]);
    fs::write(dir.path().join("b.txt"), "staged\n").unwrap();
    git_cmd(dir.path(), &["add", "b.txt"]);

    restore_paths(
        &repo,
        &restore_opts(&["b.txt"], true, false, RestoreSource::Index),
    )
    .expect("restore staged");

    assert!(git_cmd(dir.path(), &["diff", "--cached", "--name-only"])
        .trim()
        .is_empty());
    let grit_porcelain = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert_eq!(grit_porcelain, " M b.txt\n");
    git_fsck(dir.path());
}

#[test]
fn restore_source_head_parent() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("c.txt"), "one\n").unwrap();
    git_cmd(dir.path(), &["add", "c.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "first"]);
    fs::write(dir.path().join("c.txt"), "two\n").unwrap();
    git_cmd(dir.path(), &["add", "c.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "second"]);

    let tree = grit_lib::revwalk::rev_parse::resolve_revision(&repo, "HEAD~1").unwrap();
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["c.txt".into()],
            pathspec_sources: vec!["c.txt".into()],
            source: RestoreSource::Tree({
                let obj = repo.odb().read(&tree).unwrap();
                grit_lib::objects::parse_commit(&obj.data).unwrap().tree
            }),
            staged: false,
            worktree: false,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore source");

    assert_eq!(
        fs::read_to_string(dir.path().join("c.txt")).unwrap(),
        "one\n"
    );
    git_fsck(dir.path());
}

#[test]
fn restore_source_worktree_only_leaves_index_at_head() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("file"), "one\n").unwrap();
    git_cmd(dir.path(), &["add", "file"]);
    git_cmd(dir.path(), &["commit", "-qm", "first"]);
    fs::write(dir.path().join("file"), "two\n").unwrap();
    git_cmd(dir.path(), &["add", "file"]);
    git_cmd(dir.path(), &["commit", "-qm", "second"]);

    let parent = grit_lib::revwalk::rev_parse::resolve_revision(&repo, "HEAD~1").unwrap();
    let parent_tree = {
        let obj = repo.odb().read(&parent).unwrap();
        grit_lib::objects::parse_commit(&obj.data).unwrap().tree
    };
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["file".into()],
            pathspec_sources: vec!["file".into()],
            source: RestoreSource::Tree(parent_tree),
            staged: false,
            worktree: false,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore source worktree");

    assert_eq!(
        fs::read_to_string(dir.path().join("file")).unwrap(),
        "one\n"
    );
    assert_eq!(git_cmd(dir.path(), &["status", "--porcelain"]), " M file\n");
    assert!(git_cmd(dir.path(), &["diff", "--cached", "--name-only"])
        .trim()
        .is_empty());
    assert_eq!(git_cmd(dir.path(), &["diff", "--name-only"]).trim(), "file");
    git_fsck(dir.path());
}

#[test]
fn restore_deletion_skips_directory_collision() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("victim"), "v\n").unwrap();
    git_cmd(dir.path(), &["add", "victim"]);
    git_cmd(dir.path(), &["commit", "-qm", "add victim"]);
    git_cmd(dir.path(), &["rm", "victim"]);
    git_cmd(dir.path(), &["commit", "-qm", "delete victim"]);
    git_cmd(dir.path(), &["checkout", "HEAD~1", "--", "victim"]);
    fs::remove_file(dir.path().join("victim")).unwrap();
    fs::create_dir(dir.path().join("victim")).unwrap();
    fs::write(dir.path().join("victim/untracked"), "stay\n").unwrap();

    let head_tree = grit_lib::revwalk::rev_parse::resolve_revision(&repo, "HEAD").unwrap();
    let head_tree_oid = {
        let obj = repo.odb().read(&head_tree).unwrap();
        grit_lib::objects::parse_commit(&obj.data).unwrap().tree
    };
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["victim".into()],
            pathspec_sources: vec!["victim".into()],
            source: RestoreSource::Tree(head_tree_oid),
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore delete attempt");

    assert!(
        dir.path().join("victim/untracked").is_file(),
        "untracked child must survive directory collision"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("victim/untracked")).unwrap(),
        "stay\n"
    );
}

#[test]
fn restore_removes_path_missing_in_source() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("gone.txt"), "g\n").unwrap();
    git_cmd(dir.path(), &["add", "gone.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "has gone"]);
    git_cmd(dir.path(), &["rm", "gone.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "drop gone"]);

    let head_tree = grit_lib::revwalk::rev_parse::resolve_revision(&repo, "HEAD").unwrap();
    let head_obj = repo.odb().read(&head_tree).unwrap();
    let head_tree_oid = grit_lib::objects::parse_commit(&head_obj.data)
        .unwrap()
        .tree;

    use grit_lib::index::IndexEntry;
    use grit_lib::objects::ObjectKind;
    let blob = repo.odb().write(ObjectKind::Blob, b"g\n").unwrap();
    let mut index = repo.index_handle().load().unwrap();
    index.stage_file(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        size: 2,
        oid: blob,
        flags: 8,
        flags_extended: None,
        path: b"gone.txt".to_vec(),
        base_index_pos: 0,
    });
    repo.index_handle().write(&mut index).unwrap();
    fs::write(dir.path().join("gone.txt"), "orphan\n").unwrap();

    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["gone.txt".into()],
            pathspec_sources: vec!["gone.txt".into()],
            source: RestoreSource::Tree(head_tree_oid),
            staged: false,
            worktree: false,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore removal");

    assert!(!dir.path().join("gone.txt").exists());
    git_fsck(dir.path());
}

#[test]
fn restore_directory_pathspec() {
    let (dir, repo) = init_repo();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/x.txt"), "x\n").unwrap();
    fs::write(dir.path().join("sub/y.txt"), "y\n").unwrap();
    git_cmd(dir.path(), &["add", "."]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    fs::write(dir.path().join("sub/x.txt"), "x2\n").unwrap();
    fs::write(dir.path().join("sub/y.txt"), "y2\n").unwrap();

    restore_paths(
        &repo,
        &restore_opts(&["sub/"], false, false, RestoreSource::Index),
    )
    .expect("restore dir");

    assert_eq!(
        fs::read_to_string(dir.path().join("sub/x.txt")).unwrap(),
        "x\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("sub/y.txt")).unwrap(),
        "y\n"
    );
}

/// Many literal pathspecs (files, `dir`, `dir/`, prefix look-alikes) leave the same
/// worktree and status as `git restore` with the same pathspecs.
#[test]
fn restore_many_literal_pathspecs_matches_git() {
    let files = [
        "a.txt",
        "a-b.txt",
        "ab/x.txt",
        "d/one.txt",
        "d/sub/two.txt",
        "d-sibling/three.txt",
        "e/f/g/deep.txt",
        "e/f/gx.txt",
        "keep.txt",
    ];
    let specs = ["a.txt", "d", "e/f/g/", "e/f/g/deep.txt"];
    let mut worktrees = Vec::new();
    for use_git in [false, true] {
        let (dir, repo) = init_repo();
        for f in files {
            let p = dir.path().join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("{f} v1\n")).unwrap();
        }
        git_cmd(dir.path(), &["add", "."]);
        git_cmd(dir.path(), &["commit", "-qm", "init"]);
        for f in files {
            fs::write(dir.path().join(f), format!("{f} dirty\n")).unwrap();
        }
        if use_git {
            let mut args = vec!["restore", "--"];
            args.extend(specs);
            git_cmd(dir.path(), &args);
        } else {
            restore_paths(
                &repo,
                &restore_opts(&specs, false, false, RestoreSource::Index),
            )
            .expect("restore literal pathspecs");
        }
        let contents: Vec<String> = files
            .iter()
            .map(|f| fs::read_to_string(dir.path().join(f)).unwrap())
            .collect();
        let status = git_cmd(dir.path(), &["status", "--porcelain"]);
        git_fsck(dir.path());
        worktrees.push((contents, status));
    }
    assert_eq!(worktrees[0], worktrees[1]);
    assert!(worktrees[0].1.contains(" M d-sibling/three.txt"));
    assert!(worktrees[0].1.contains(" M e/f/gx.txt"));
    assert!(!worktrees[0].1.contains("d/sub/two.txt"));

    let (dir, repo) = init_repo();
    fs::write(dir.path().join("a.txt"), "v1\n").unwrap();
    git_cmd(dir.path(), &["add", "."]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    let err = restore_paths(
        &repo,
        &restore_opts(&["a.txt", "missing/"], false, false, RestoreSource::Index),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::PathspecNoMatch { ref spec } if spec == "missing/"
    ));
}

#[test]
fn restore_pathspec_no_match_errors() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("z.txt"), "z\n").unwrap();
    git_cmd(dir.path(), &["add", "z.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "c"]);

    let err = restore_paths(
        &repo,
        &restore_opts(&["missing.txt"], false, false, RestoreSource::Index),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::PathspecNoMatch { .. }
    ));
}

#[test]
fn restore_worktree_pathspec_no_match_when_only_in_head() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("f"), "content\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "add f"]);
    git_cmd(dir.path(), &["rm", "--cached", "f"]);

    let err = restore_paths(
        &repo,
        &restore_opts(&["f"], false, false, RestoreSource::Index),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::PathspecNoMatch { .. }
    ));

    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args(["restore", "--", "f"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git restore");
    assert!(
        !git_out.status.success(),
        "git restore should fail for HEAD-only path: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
}

#[test]
fn restore_pathspec_exclude_skips_excluded_paths() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("keep"), "keep-v1\n").unwrap();
    fs::write(dir.path().join("skip"), "skip-v1\n").unwrap();
    git_cmd(dir.path(), &["add", "keep", "skip"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    fs::write(dir.path().join("keep"), "keep-dirty\n").unwrap();
    fs::write(dir.path().join("skip"), "skip-dirty\n").unwrap();

    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec![".".into(), ":!skip".into()],
            pathspec_sources: vec![".".into(), ":!skip".into()],
            source: RestoreSource::Index,
            staged: false,
            worktree: false,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore with exclude");

    assert_eq!(
        fs::read_to_string(dir.path().join("keep")).unwrap(),
        "keep-v1\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("skip")).unwrap(),
        "skip-dirty\n"
    );

    fs::write(dir.path().join("keep"), "keep-dirty\n").unwrap();
    fs::write(dir.path().join("skip"), "skip-dirty\n").unwrap();
    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args(["restore", ".", ":!skip"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git restore");
    assert!(git_out.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("keep")).unwrap(),
        "keep-v1\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("skip")).unwrap(),
        "skip-dirty\n"
    );
    git_fsck(dir.path());
}

#[test]
fn restore_applies_index_gitattributes_without_worktree_copy() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
    fs::write(dir.path().join("data.txt"), b"hello\n").unwrap();
    git_cmd(dir.path(), &["add", ".gitattributes", "data.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "attrs"]);
    fs::remove_file(dir.path().join(".gitattributes")).unwrap();
    fs::write(dir.path().join("data.txt"), b"dirty\n").unwrap();

    restore_paths(
        &repo,
        &restore_opts(&["data.txt"], false, false, RestoreSource::Index),
    )
    .expect("restore with index attrs");

    assert_eq!(fs::read(dir.path().join("data.txt")).unwrap(), b"hello\r\n");

    fs::write(dir.path().join("data.txt"), b"dirty\n").unwrap();
    git_cmd(dir.path(), &["restore", "data.txt"]);
    assert_eq!(fs::read(dir.path().join("data.txt")).unwrap(), b"hello\r\n");
    git_fsck(dir.path());
}

#[test]
fn restore_staged_matches_path_only_in_head() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("f"), "content\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "add f"]);
    git_cmd(dir.path(), &["rm", "--cached", "f"]);

    restore_paths(
        &repo,
        &restore_opts(&["f"], true, false, RestoreSource::Index),
    )
    .expect("restore staged from HEAD");

    assert!(!git_cmd(dir.path(), &["ls-files", "f"]).trim().is_empty());
    git_fsck(dir.path());
}

#[test]
fn restore_leaves_untracked_alone() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("tracked.txt"), "t\n").unwrap();
    git_cmd(dir.path(), &["add", "tracked.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "c"]);
    fs::write(dir.path().join("tracked.txt"), "dirty\n").unwrap();
    fs::write(dir.path().join("untracked.txt"), "u\n").unwrap();

    restore_paths(
        &repo,
        &restore_opts(&["."], false, false, RestoreSource::Index),
    )
    .expect("restore");

    assert_eq!(
        fs::read_to_string(dir.path().join("untracked.txt")).unwrap(),
        "u\n"
    );
}

#[test]
fn restore_symlink_and_executable() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("run.sh"), "#!/bin/sh\n").unwrap();
    symlink("run.sh", dir.path().join("link.sh")).unwrap();
    git_cmd(dir.path(), &["add", "run.sh", "link.sh"]);
    git_cmd(dir.path(), &["update-index", "--chmod=+x", "run.sh"]);
    git_cmd(dir.path(), &["commit", "-qm", "modes"]);
    fs::write(dir.path().join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    fs::remove_file(dir.path().join("link.sh")).unwrap();
    symlink("run.sh", dir.path().join("link.sh")).unwrap();

    restore_paths(
        &repo,
        &restore_opts(&["run.sh", "link.sh"], false, false, RestoreSource::Index),
    )
    .expect("restore modes");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::symlink_metadata(dir.path().join("run.sh")).unwrap();
        assert_ne!(meta.permissions().mode() & 0o111, 0);
    }
    assert!(dir
        .path()
        .join("link.sh")
        .symlink_metadata()
        .unwrap()
        .is_symlink());
    let idx = repo.index_handle().load().unwrap();
    let run = idx.get(b"run.sh", 0).unwrap();
    let link = idx.get(b"link.sh", 0).unwrap();
    assert_eq!(run.mode, MODE_EXECUTABLE);
    assert_eq!(link.mode, MODE_SYMLINK);
    git_fsck(dir.path());
    git_cmd(dir.path(), &["ls-files", "-s"]);
}

#[test]
fn git_staged_then_grit_restore_staged() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("m.txt"), "a\n").unwrap();
    git_cmd(dir.path(), &["add", "m.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "c1"]);
    fs::write(dir.path().join("m.txt"), "b\n").unwrap();
    git_cmd(dir.path(), &["add", "m.txt"]);

    restore_paths(
        &repo,
        &restore_opts(&["m.txt"], true, false, RestoreSource::Index),
    )
    .expect("grit restore --staged");

    assert!(git_cmd(dir.path(), &["diff", "--cached", "--name-only"])
        .trim()
        .is_empty());
    let ls = git_cmd(dir.path(), &["ls-files", "-s"]);
    assert!(ls.contains("m.txt"));
    git_fsck(dir.path());
}

#[test]
fn restore_index_readable_by_git_ls_files() {
    let (dir, repo) = init_repo();
    fs::write(dir.path().join("i.txt"), "1\n").unwrap();
    git_cmd(dir.path(), &["add", "i.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "c"]);
    fs::write(dir.path().join("i.txt"), "2\n").unwrap();

    restore_paths(
        &repo,
        &restore_opts(&["i.txt"], false, false, RestoreSource::Index),
    )
    .expect("restore");

    let ls = git_cmd(dir.path(), &["ls-files", "-s"]);
    assert!(ls.contains("100644"));
    git_fsck(dir.path());
}
