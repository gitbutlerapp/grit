//! Shallow `deepen` on a single-commit repository (system `git` parity).

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::shallow_grafts_for_upload_pack_deepen;

fn init_single_commit_bare(dir: &Path) -> (Repository, ObjectId) {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let repo = Repository::open(dir, &RepositoryOptions::empty()).expect("open");
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
    let commit_body = format!(
        "tree {}\nauthor T <t@x> 1 +0000\ncommitter T <t@x> 1 +0000\n\nonly\n",
        tree.to_hex()
    );
    let tip = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    grit_lib::refs::write_ref(dir, "refs/heads/main", &tip).expect("ref");
    (repo, tip)
}

fn git_clone_depth(bare: &Path, dest: &Path, depth: u32) -> bool {
    Command::new("git")
        .args([
            "clone",
            "-q",
            "--depth",
            &depth.to_string(),
            &format!("file://{}", bare.display()),
            dest.to_str().expect("utf8 dest"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn clone_has_shallow_file(clone_git_dir: &Path) -> bool {
    clone_git_dir.join("shallow").is_file()
}

#[test]
fn deepen_one_on_root_commit_returns_shallow_border() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, tip) = init_single_commit_bare(dir.path());

    let borders = shallow_grafts_for_upload_pack_deepen(&repo, &[tip], &[], 1);
    assert_eq!(
        borders,
        vec![tip],
        "deepen 1 on a single commit must advertise that commit as shallow"
    );
}

#[test]
fn deepen_beyond_history_does_not_advertise_shallow_border() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, tip) = init_single_commit_bare(dir.path());

    let borders = shallow_grafts_for_upload_pack_deepen(&repo, &[tip], &[], 5);
    assert!(
        borders.is_empty(),
        "deepen larger than history must not mark the root shallow (complete history)"
    );
}

#[test]
fn single_commit_shallow_borders_match_git_clone_depth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, tip) = init_single_commit_bare(dir.path());

    let grit_depth_one = shallow_grafts_for_upload_pack_deepen(&repo, &[tip], &[], 1);
    let grit_depth_five = shallow_grafts_for_upload_pack_deepen(&repo, &[tip], &[], 5);
    assert_eq!(grit_depth_one.len(), 1);
    assert!(grit_depth_five.is_empty());

    let d1 = dir.path().join("git-d1");
    let d5 = dir.path().join("git-d5");
    if !git_clone_depth(dir.path(), &d1, 1) || !git_clone_depth(dir.path(), &d5, 5) {
        eprintln!("SKIP: system git clone unavailable");
        return;
    }
    assert!(
        clone_has_shallow_file(&d1.join(".git")),
        "git clone --depth 1 must write .git/shallow for a one-commit repo"
    );
    assert!(
        !clone_has_shallow_file(&d5.join(".git")),
        "git clone --depth 5 must not write .git/shallow when history is complete"
    );
}

#[test]
fn deepen_with_annotated_tag_want_borders_the_tagged_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, tip) = init_single_commit_bare(dir.path());
    let tag_body = format!(
        "object {}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nv1\n",
        tip.to_hex()
    );
    let tag = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tag, tag_body.as_bytes())
        .expect("tag");

    assert_eq!(
        shallow_grafts_for_upload_pack_deepen(&repo, &[tag], &[], 1),
        vec![tip],
        "a tag want is counted from the commit it names"
    );
    assert_eq!(
        shallow_grafts_for_upload_pack_deepen(&repo, &[tip, tag], &[], 1),
        vec![tip]
    );
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
    assert!(
        shallow_grafts_for_upload_pack_deepen(&repo, &[tree], &[], 1).is_empty(),
        "a non-commit want has no history to cut"
    );
}
