//! Linked worktree admin layout and per-worktree refs vs system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::repo::{init_worktree_config, Repository};
use grit_lib::worktree::worktree_ref::{parse_worktree_ref, resolve_ref_storage, RefWorktreeType};
use grit_lib::worktree::{
    add_linked_worktree, copy_filtered_worktree_config, list_worktrees, remove_linked_worktree,
    AddLinkedWorktreeOptions,
};
use grit_test_support::git;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git_cmd(root: &Path, args: &[&str]) -> String {
    git(root, args)
}

fn init_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn open_grit(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover")
}

fn assert_fsck_strict(root: &Path) {
    let out = git_cmd(root, &["fsck", "--strict"]);
    assert!(
        !out.to_lowercase().contains("error"),
        "fsck --strict must be clean:\n{out}"
    );
}

#[test]
fn t2400_worktree_admin_dir_roundtrip_with_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("t"), "1\n").unwrap();
    git_cmd(&main, &["add", "t"]);
    git_cmd(&main, &["commit", "-qm", "seed"]);

    let grit_linked = tmp.path().join("grit-linked");
    add_linked_worktree(
        &open_grit(&main),
        &AddLinkedWorktreeOptions {
            path: grit_linked.clone(),
            branch: Some("grit-wt".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("grit add");

    let git_linked = tmp.path().join("git-linked");
    git_cmd(
        &main,
        &[
            "worktree",
            "add",
            git_linked.to_str().unwrap(),
            "-b",
            "git-wt",
        ],
    );

    for linked in [&grit_linked, &git_linked] {
        assert!(linked.join(".git").is_file(), "gitfile worktree");
        let porcelain = git_cmd(linked, &["worktree", "list", "--porcelain"]);
        assert!(porcelain.contains("worktree"));
    }

    let grit_list = list_worktrees(&open_grit(&main)).expect("list");
    assert_eq!(grit_list.len(), 3, "main + grit + git linked worktrees");
    assert!(
        grit_list.iter().any(|e| e.path.ends_with("grit-linked")),
        "grit list must include grit-linked"
    );

    let git_list = git_cmd(&main, &["worktree", "list"]);
    assert!(git_list.contains("grit-linked"));
    assert_fsck_strict(&main);
}

#[test]
fn t2404_worktree_config_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "x\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    init_worktree_config(main.join(".git").as_path()).expect("init worktree config");
    fs::write(
        main.join(".git/config.worktree"),
        "[wtsection]\n\tfrom-common = yes\n[core]\n\tbare = false\n\tworktree = /ignored\n",
    )
    .expect("write config.worktree");

    let linked = tmp.path().join("linked");
    add_linked_worktree(
        &open_grit(&main),
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some("cfg-wt".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("add");

    let admin = list_worktrees(&open_grit(&main))
        .expect("list")
        .into_iter()
        .find(|e| e.path == linked.canonicalize().unwrap_or(linked.clone()))
        .expect("linked entry")
        .admin_dir;
    let wt_config = fs::read_to_string(admin.join("config.worktree")).expect("read wt config");
    assert!(wt_config.contains("from-common"));
    assert!(
        !wt_config.contains("worktree ="),
        "core.worktree must be stripped from linked admin config"
    );

    let repo = open_grit(&linked);
    let cfg = repo.config().expect("config");
    assert_eq!(
        cfg.get("wtsection.from-common").as_deref(),
        Some("yes"),
        "linked worktree must load config.worktree"
    );
    assert_fsck_strict(&main);
}

#[test]
fn t2402_worktree_list_matches_git_porcelain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("a"), "a\n").unwrap();
    git_cmd(&main, &["add", "a"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let linked = tmp.path().join("linked");
    add_linked_worktree(
        &open_grit(&main),
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some("list-wt".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("add");

    let grit_paths: Vec<_> = list_worktrees(&open_grit(&main))
        .expect("list")
        .into_iter()
        .map(|e| e.path.canonicalize().unwrap_or(e.path))
        .collect();
    let git_out = Command::new("git")
        .current_dir(&main)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git");
    assert!(git_out.status.success());
    let text = String::from_utf8_lossy(&git_out.stdout);
    for path in grit_paths {
        let p = path.to_string_lossy();
        assert!(
            text.contains(p.as_ref()),
            "git porcelain must list grit worktree {p}"
        );
    }
}

#[test]
fn per_worktree_refs_resolve_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "f\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);
    let tip = git_cmd(&main, &["rev-parse", "HEAD"]).trim().to_owned();

    let linked = tmp.path().join("linked");
    add_linked_worktree(
        &open_grit(&main),
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some("ref-wt".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("add");

    let linked_repo = open_grit(&linked);
    write_ref(
        linked_repo.git_dir(),
        "refs/worktree/exclusive",
        &tip.parse().expect("oid"),
    )
    .expect("write per-worktree ref");

    let git_oid = git_cmd(&linked, &["rev-parse", "refs/worktree/exclusive"])
        .trim()
        .to_owned();
    let grit_oid = resolve_ref(linked_repo.git_dir(), "refs/worktree/exclusive")
        .expect("resolve")
        .to_hex();
    assert_eq!(grit_oid, git_oid);

    let (kind, bare, _) = parse_worktree_ref("main-worktree/HEAD");
    assert_eq!(kind, RefWorktreeType::Main);
    assert_eq!(bare, "HEAD");
    let (store, stor) = resolve_ref_storage(main.join(".git").as_path(), "main-worktree/HEAD");
    assert!(store.ends_with(".git"));
    assert_eq!(stor, "HEAD");

    fs::create_dir_all(tmp.path().join("admin")).expect("admin");
    copy_filtered_worktree_config(
        main.join(".git").as_path(),
        tmp.path().join("admin").as_path(),
    )
    .expect("copy filtered worktree config");
}

#[test]
fn t2401_remove_linked_worktree_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "x\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let grit_wt = tmp.path().join("grit-rm");
    add_linked_worktree(
        &open_grit(&main),
        &AddLinkedWorktreeOptions {
            path: grit_wt.clone(),
            branch: Some("rm-wt".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("add");

    remove_linked_worktree(&open_grit(&main), &grit_wt, false).expect("grit remove");
    assert!(!grit_wt.exists());
    assert_eq!(list_worktrees(&open_grit(&main)).unwrap().len(), 1);

    let git_wt = tmp.path().join("git-rm");
    git_cmd(
        &main,
        &[
            "worktree",
            "add",
            git_wt.to_str().unwrap(),
            "-b",
            "git-rm-wt",
        ],
    );
    git_cmd(&main, &["worktree", "remove", git_wt.to_str().unwrap()]);
    assert_fsck_strict(&main);
}
