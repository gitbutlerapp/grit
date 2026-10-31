//! Local and `file://` push, fetch and clone transfer only the objects reachable
//! from the requested refs, like the wire protocol, even when the sender keeps
//! everything in one pack. Only a clone from a plain path links the whole
//! source object store (Git's `clone --local`). Object counts and dangling
//! objects are checked with the system `git`.

use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::remote::Remote;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_work_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.email", "a@b"]);
    git(dir, &["config", "user.name", "n"]);
}

fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-qm", msg]);
}

/// `main` with one commit plus a `secret` branch commit, all in one pack.
///
/// `main` reaches exactly 3 objects (commit, tree, blob).
fn repo_with_secret_branch(dir: &Path) {
    init_work_repo(dir);
    commit_file(dir, "a", "1\n", "one");
    git(dir, &["checkout", "-q", "-b", "secret"]);
    commit_file(dir, "s.txt", "s\n", "SECRET");
    git(dir, &["checkout", "-q", "main"]);
    git(dir, &["gc", "-q"]);
}

fn object_count(git_dir: &Path) -> usize {
    git(
        git_dir,
        &["cat-file", "--batch-all-objects", "--batch-check"],
    )
    .lines()
    .count()
}

fn object_types(git_dir: &Path) -> Vec<String> {
    git(
        git_dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objecttype)",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

fn assert_fsck_no_dangling(git_dir: &Path) {
    let out = grit_test_support::git_cmd(&["fsck", "--strict"])
        .in_dir(git_dir)
        .exec();
    assert!(out.ok(), "git fsck failed: {}{}", out.stdout, out.stderr);
    let report = format!("{}{}", out.stdout, out.stderr);
    assert!(
        !report.contains("dangling"),
        "receiver holds objects unreachable from its refs:\n{report}"
    );
}

fn pack_basenames(git_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(git_dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".pack"))
        .collect();
    names.sort();
    names
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn open_work_repo(dir: &Path) -> Repository {
    Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir),
    )
    .expect("open repo")
}

fn fetch_heads(dest: &Path, url: &str) {
    init_work_repo(dest);
    let repo = open_work_repo(dest);
    Remote::from_url(url)
        .expect("url")
        .fetch(
            &repo,
            {
                let mut o = FetchOptions::default();
                o.refspecs = vec!["+refs/heads/*:refs/remotes/o/*".to_owned()];
                o.tags = TagMode::None;
                o
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch");
}

fn bare(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "--bare", "-b", "main", "."]);
    dir.to_path_buf()
}

#[test]
fn file_url_push_sends_only_objects_reachable_from_pushed_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    repo_with_secret_branch(&src);
    let dst = bare(&tmp.path().join("dst.git"));
    let git_dst = bare(&tmp.path().join("git-dst.git"));

    let main = ObjectId::from_hex(git(&src, &["rev-parse", "main"]).trim()).unwrap();
    let repo = open_work_repo(&src);
    let outcome = Remote::from_url(&file_url(&dst))
        .expect("url")
        .push(
            &repo,
            &[PushRefSpec {
                src: Some(main),
                dst: "refs/heads/main".to_owned(),
                ..Default::default()
            }],
            PushOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("push");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);

    git(&src, &["push", "-q", &file_url(&git_dst), "main"]);
    assert_eq!(object_count(&git_dst), 3);
    assert_eq!(object_count(&dst), object_count(&git_dst));
    assert_fsck_no_dangling(&dst);
}

#[test]
fn file_url_fetch_skips_objects_of_unrequested_hidden_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    repo_with_secret_branch(&src);
    git(&src, &["update-ref", "refs/hidden/wip", "secret"]);
    git(&src, &["branch", "-D", "-q", "secret"]);
    git(&src, &["gc", "-q"]);

    let dest = tmp.path().join("dest");
    fetch_heads(&dest, &file_url(&src));
    let dest_git = dest.join(".git");
    assert_eq!(object_count(&dest_git), 3);
    assert_fsck_no_dangling(&dest_git);
}

#[test]
fn file_url_fetch_without_tags_receives_no_tag_objects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    init_work_repo(&src);
    commit_file(&src, "a", "1\n", "one");
    git(&src, &["tag", "-a", "-m", "release", "v1"]);
    commit_file(&src, "b", "2\n", "two");
    git(&src, &["gc", "-q"]);

    let dest = tmp.path().join("dest");
    fetch_heads(&dest, &file_url(&src));
    let dest_git = dest.join(".git");
    assert!(
        !object_types(&dest_git).iter().any(|t| t == "tag"),
        "TagMode::None must not transfer annotated tag objects"
    );
    assert_eq!(object_count(&dest_git), 6);
    assert_fsck_no_dangling(&dest_git);
}

#[test]
fn file_url_fetch_links_only_packs_inside_the_reachable_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    init_work_repo(&src);
    commit_file(&src, "a", "1\n", "one");
    git(&src, &["repack", "-q", "-d"]);
    let main_pack = pack_basenames(&src.join(".git"));
    assert_eq!(main_pack.len(), 1);
    git(&src, &["checkout", "-q", "-b", "wip"]);
    commit_file(&src, "w", "w\n", "wip");
    git(&src, &["update-ref", "refs/hidden/wip", "wip"]);
    git(&src, &["checkout", "-q", "main"]);
    git(&src, &["branch", "-D", "-q", "wip"]);
    git(&src, &["repack", "-q", "-d"]);
    assert_eq!(pack_basenames(&src.join(".git")).len(), 2);

    let dest = tmp.path().join("dest");
    fetch_heads(&dest, &file_url(&src));
    let dest_git = dest.join(".git");
    assert_eq!(
        pack_basenames(&dest_git),
        main_pack,
        "the pack holding only main's objects is linked, the hidden ref's pack is not"
    );
    assert_eq!(object_count(&dest_git), 3);
    assert_fsck_no_dangling(&dest_git);
}

#[test]
fn file_url_clone_matches_git_and_plain_path_clone_links_whole_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    repo_with_secret_branch(&src);
    git(&src, &["update-ref", "refs/hidden/wip", "secret"]);
    git(&src, &["branch", "-D", "-q", "secret"]);
    git(&src, &["gc", "-q"]);
    let src_packs = pack_basenames(&src.join(".git"));
    assert_eq!(src_packs.len(), 1);

    let clone = |url: String, dest: PathBuf| {
        clone_repo(
            CloneOptions::new(url, dest),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("clone");
    };

    let url_clone = tmp.path().join("url-clone");
    clone(file_url(&src), url_clone.clone());
    let git_url_clone = tmp.path().join("git-url-clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            &file_url(&src),
            git_url_clone.to_str().unwrap(),
        ],
    );
    assert_eq!(object_count(&git_url_clone.join(".git")), 3);
    assert_eq!(object_count(&url_clone.join(".git")), 3);
    assert_fsck_no_dangling(&url_clone.join(".git"));

    // A plain-path clone links the source pack wholesale, like `git clone --local`.
    let path_clone = tmp.path().join("path-clone");
    clone(src.to_string_lossy().into_owned(), path_clone.clone());
    let git_path_clone = tmp.path().join("git-path-clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            src.to_str().unwrap(),
            git_path_clone.to_str().unwrap(),
        ],
    );
    assert_eq!(pack_basenames(&path_clone.join(".git")), src_packs);
    assert_eq!(
        object_count(&path_clone.join(".git")),
        object_count(&git_path_clone.join(".git"))
    );
    git(&path_clone, &["fsck", "--strict"]);
}
