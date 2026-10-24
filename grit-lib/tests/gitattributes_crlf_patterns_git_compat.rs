//! Issue #1698: anchored, bracket-class, and dir-scoped gitattributes with `core.autocrlf`.

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn blob_size(repo: &std::path::Path, path: &str) -> usize {
    let hex = git(repo, &["rev-parse", &format!(":{path}")]);
    let out = git(repo, &["cat-file", "-s", hex.trim()]);
    out.trim().parse().expect("size")
}

fn init_issue_repro(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@e.c"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "core.autocrlf", "true"]);
    fs::write(
        root.join(".gitattributes"),
        "/top.raw -text\n*.[ch] -text\nd/*.q -text\n",
    )
    .expect("attrs");
    fs::create_dir_all(root.join("d/sub")).expect("mkdir");
    for (path, bytes) in [
        ("top.raw", b"A\r\nB\r\n" as &[u8]),
        ("m.c", b"A\r\nB\r\n"),
        ("d/x.q", b"A\r\nB\r\n"),
        ("d/sub/y.q", b"A\r\nB\r\n"),
    ] {
        fs::write(root.join(path), bytes).expect("write");
    }
}

#[test]
fn grit_add_honours_gitattributes_with_autocrlf_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_issue_repro(root);

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");

    stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit add");

    let expected = [
        ("top.raw", 6_usize),
        ("m.c", 6),
        ("d/x.q", 6),
        ("d/sub/y.q", 4),
    ];
    let index = repo.index_handle().load().expect("index");
    for (path, size) in expected {
        let entry = index
            .get(path.as_bytes(), 0)
            .unwrap_or_else(|| panic!("missing {path}"));
        let blob = repo.odb().read(&entry.oid).expect("blob");
        assert_eq!(blob.data.len(), size, "{path}: grit staged blob size");
    }

    git(root, &["add", "-A"]);
    for (path, size) in expected {
        assert_eq!(
            blob_size(root, path),
            size,
            "{path}: must match system git index blob size"
        );
    }
}

#[test]
fn nested_anchored_binary_pattern_blocks_autocrlf_on_checkout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@e.c"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "core.autocrlf", "true"]);
    fs::create_dir_all(root.join("sub")).expect("mkdir");
    fs::write(root.join("sub/.gitattributes"), "/bin-* binary\n").expect("nested attrs");
    fs::write(root.join("sub/bin-1"), b"a\nb\n").expect("file");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c1"]);

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    repo.reload_config().expect("reload");

    fs::remove_file(root.join("sub/bin-1")).expect("remove");
    let tree_hex = git(root, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let tree = grit_lib::objects::ObjectId::from_hex(&tree_hex).expect("tree");
    grit_lib::porcelain::checkout::checkout_between_trees(&repo, None, &tree).expect("checkout");

    let bytes = fs::read(root.join("sub/bin-1")).expect("read");
    assert_eq!(
        bytes.len(),
        4,
        "anchored /bin-* must stay binary on checkout"
    );
    let porcelain = git(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git status must be clean after grit checkout, got:\n{porcelain}"
    );
}
