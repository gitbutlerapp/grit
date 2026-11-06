//! Object reads in repositories with a multi-pack-index: loose objects, packs the index
//! does not cover, and covered packs holding objects the index does not list.

#[allow(clippy::unwrap_used, clippy::expect_used)]
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0")
        .env("GIT_CONFIG_KEY_1", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_1", "false");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = git_cmd(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child.stdin.take().expect("stdin").write_all(input).unwrap();
    let out = child.wait_with_output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn hash_blob(dir: &Path, content: &str) -> ObjectId {
    git_stdin(dir, &["hash-object", "-w", "--stdin"], content.as_bytes())
        .trim()
        .parse()
        .unwrap()
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn loose_path(dir: &Path, oid: &ObjectId) -> PathBuf {
    let hex = oid.to_hex();
    dir.join(".git/objects").join(&hex[..2]).join(&hex[2..])
}

fn pack_files(dir: &Path) -> Vec<PathBuf> {
    let mut packs: Vec<PathBuf> = std::fs::read_dir(dir.join(".git/objects/pack"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "pack"))
        .collect();
    packs.sort();
    packs
}

/// Every object git lists must read back through grit with the same type, size and bytes.
fn assert_all_objects_read_like_git(dir: &Path) {
    let repo = open(dir);
    let listing = git(
        dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ],
    );
    assert!(!listing.is_empty());
    for line in listing.lines() {
        let mut fields = line.split(' ');
        let oid: ObjectId = fields.next().unwrap().parse().unwrap();
        let kind = fields.next().unwrap();
        let size: u64 = fields.next().unwrap().parse().unwrap();
        let info = repo.odb().read_info(&oid).expect("read_info");
        assert_eq!(info.kind.as_str(), kind, "{oid}");
        assert_eq!(info.size, size, "{oid}");
        let obj = repo.odb().read(&oid).expect("read");
        assert_eq!(obj.kind.as_str(), kind, "{oid}");
        let expected = Command::new("git")
            .current_dir(dir)
            .args(["cat-file", kind, &oid.to_hex()])
            .output()
            .expect("cat-file");
        assert_eq!(obj.data, expected.stdout, "{oid}");
    }
}

#[test]
fn loose_and_uncovered_pack_objects_read_with_midx_present() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..3 {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("file {i}\n")).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(dir, &["repack", "-adq"]);
    git(dir, &["multi-pack-index", "write"]);

    let uncovered = hash_blob(dir, "only in a pack written after the midx\n");
    git_stdin(
        dir,
        &["pack-objects", "-q", ".git/objects/pack/pack"],
        format!("{uncovered}\n").as_bytes(),
    );
    std::fs::remove_file(loose_path(dir, &uncovered)).unwrap();

    let loose = hash_blob(dir, "still loose\n");
    git(
        dir,
        &["tag", "-a", "loose-tag", "-m", "loose tag object", "HEAD"],
    );
    assert!(loose_path(dir, &loose).is_file());
    assert_eq!(pack_files(dir).len(), 2);

    assert_all_objects_read_like_git(dir);
    let repo = open(dir);
    assert_eq!(
        repo.odb().read(&uncovered).expect("uncovered").kind,
        ObjectKind::Blob
    );
}

#[test]
fn object_missing_from_midx_is_found_in_covered_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "seed"]);
    git(dir, &["repack", "-adq"]);
    git(dir, &["multi-pack-index", "write"]);
    let covered = pack_files(dir).pop().expect("covered pack");

    // Rewrite the covered pack (under its old name) so it also holds an object the
    // multi-pack-index has never heard of.
    let extra = hash_blob(dir, "listed by the pack index but not the midx\n");
    let all = git(dir, &["rev-list", "--objects", "--all"]);
    let scratch = tempfile::tempdir().unwrap();
    let base = scratch.path().join("pack");
    let input = format!("{all}{extra}\n");
    let hash = git_stdin(
        dir,
        &["pack-objects", "-q", base.to_str().unwrap()],
        input.as_bytes(),
    );
    let new_base = scratch.path().join(format!("pack-{}", hash.trim()));
    for ext in ["pack", "idx"] {
        let dest = covered.with_extension(ext);
        std::fs::remove_file(&dest).unwrap();
        std::fs::copy(new_base.with_extension(ext), &dest).unwrap();
    }
    let _ = std::fs::remove_file(covered.with_extension("rev"));
    std::fs::remove_file(loose_path(dir, &extra)).unwrap();

    let repo = open(dir);
    let info = repo
        .odb()
        .read_info(&extra)
        .expect("read_info through rescue");
    assert_eq!(info.kind, ObjectKind::Blob);
    let obj = repo.odb().read(&extra).expect("read through rescue");
    assert_eq!(obj.data, b"listed by the pack index but not the midx\n");
}
