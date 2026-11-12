//! Linked worktree admin layout and per-worktree refs vs system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::{init_worktree_config, Repository};
use grit_lib::test_support::worktree::copy_filtered_worktree_config;
use grit_lib::worktree::worktree_ref::{parse_worktree_ref, resolve_ref_storage, RefWorktreeType};
use grit_lib::worktree::{AddLinkedWorktreeOptions, WorktreeBranch};
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
    open_grit(&main)
        .worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(grit_linked.clone())
                .with_branch(Some(WorktreeBranch::Create("grit-wt".into())))
                .with_start_point(Some("HEAD".into())),
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

    let grit_list = open_grit(&main.worktrees().list()).expect("list");
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
    open_grit(&main)
        .worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(linked.clone())
                .with_branch(Some(WorktreeBranch::Create("cfg-wt".into())))
                .with_start_point(Some("HEAD".into())),
        )
        .expect("add");

    let admin = open_grit(&main.worktrees().list())
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
    let cfg = repo.config().snapshot().expect("config");
    assert_eq!(
        cfg.get("wtsection.from-common").unwrap().as_deref(),
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
    open_grit(&main)
        .worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(linked.clone())
                .with_branch(Some(WorktreeBranch::Create("list-wt".into())))
                .with_start_point(Some("HEAD".into())),
        )
        .expect("add");

    let grit_paths: Vec<_> = open_grit(&main.worktrees().list())
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
    open_grit(&main)
        .worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(linked.clone())
                .with_branch(Some(WorktreeBranch::Create("ref-wt".into())))
                .with_start_point(Some("HEAD".into())),
        )
        .expect("add");

    let linked_repo = open_grit(&linked);
    grit_test_support::refs::set_ref(
        &linked_repo,
        "refs/worktree/exclusive",
        &tip.parse().expect("oid"),
    )
    .expect("write per-worktree ref");

    let git_oid = git_cmd(&linked, &["rev-parse", "refs/worktree/exclusive"])
        .trim()
        .to_owned();
    let grit_oid = linked_repo
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/worktree/exclusive")
                .expect("valid ref name"),
        )
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
    open_grit(&main)
        .worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(grit_wt.clone())
                .with_branch(Some(WorktreeBranch::Create("rm-wt".into())))
                .with_start_point(Some("HEAD".into())),
        )
        .expect("add");

    open_grit(&main)
        .worktrees()
        .remove(&grit_wt, false)
        .expect("grit remove");
    assert!(!grit_wt.exists());
    assert_eq!(open_grit(&main).worktrees().list().unwrap().len(), 1);

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

fn grit_ref_listing(repo: &Repository) -> Vec<String> {
    use grit_lib::refs::listing::{RefNamespace, RefQuery, RefQueryOptions};
    repo.references()
        .query(
            &RefQuery::default().with_namespaces(vec![RefNamespace::Custom("refs/".into())]),
            &RefQueryOptions::default(),
        )
        .expect("query refs")
        .into_iter()
        .map(|r| r.full_name)
        .collect()
}

fn git_ref_listing(root: &Path) -> Vec<String> {
    git_cmd(root, &["for-each-ref", "--format=%(refname)", "refs/"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn grit_reflog_refs(repo: &Repository) -> Vec<String> {
    let mut names = Vec::new();
    repo.ref_store()
        .for_each_reflog_ref(&mut |name| {
            names.push(name.to_owned());
            std::ops::ControlFlow::Continue(())
        })
        .expect("reflog refs");
    names
}

/// Loose and packed shared refs plus per-worktree refs list like `git for-each-ref`
/// from both the main and a linked worktree, each name exactly once.
#[test]
fn ref_listing_from_main_and_linked_worktrees_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "f\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);
    for i in 0..5 {
        git_cmd(&main, &["branch", &format!("packed-{i}")]);
    }
    git_cmd(&main, &["pack-refs", "--all"]);
    for i in 0..5 {
        git_cmd(&main, &["branch", &format!("loose-{i}")]);
    }
    git_cmd(&main, &["tag", "v1"]);
    git_cmd(&main, &["update-ref", "refs/bisect/main-only", "HEAD"]);

    let linked = tmp.path().join("linked");
    git_cmd(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "wt",
        ],
    );
    git_cmd(
        &linked,
        &["update-ref", "refs/worktree/linked-only", "HEAD"],
    );
    git_cmd(&linked, &["update-ref", "refs/bisect/linked-only", "HEAD"]);

    for root in [&main, &linked] {
        let repo = open_grit(root);
        let grit = grit_ref_listing(&repo);
        assert_eq!(
            grit,
            git_ref_listing(root),
            "listing from {}",
            root.display()
        );
        let mut unique = grit.clone();
        unique.dedup();
        assert_eq!(unique.len(), grit.len(), "duplicate names: {grit:?}");

        let reflogs = grit_reflog_refs(&repo);
        assert!(
            reflogs.iter().any(|r| r == "refs/heads/loose-0"),
            "shared reflogs visible from {}: {reflogs:?}",
            root.display()
        );
        let mut sorted = reflogs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), reflogs.len(), "duplicate reflog names");
    }
    let linked_refs = grit_ref_listing(&open_grit(&linked));
    assert!(linked_refs.contains(&"refs/worktree/linked-only".to_owned()));
    assert!(linked_refs.contains(&"refs/bisect/linked-only".to_owned()));
    assert!(!linked_refs.contains(&"refs/bisect/main-only".to_owned()));
}
