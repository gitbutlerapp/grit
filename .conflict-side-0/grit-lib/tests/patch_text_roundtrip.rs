//! `write_patch` / `--patch-text` round-trip vs system `git diff --binary` and `git apply`.

use std::io::Cursor;
use std::process::Command;

use grit_lib::apply::{materialize_binary_patch_forward, parse_patch};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_diff::read_blob_at_path;
use grit_lib::diff::{
    diff_commits_with_renames_and_policy, tree_diff_structured_parallel, write_patch_structured,
    DiffOptions, PatchTextOptions,
};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use tempfile::TempDir;

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_tree(dir: &std::path::Path, rev: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", &format!("{rev}^{{tree}}")])
        .output()
        .expect("git rev-parse tree");
    assert!(out.status.success());
    ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("tree oid")
}

fn git_diff_binary(dir: &std::path::Path, base: &str, tip: &str) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["diff", "--binary", "-M50", base, tip])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git diff");
    assert!(
        out.status.success(),
        "git diff: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn grit_patch_structured(repo: &Repository, base: &str, tip: &str) -> Vec<u8> {
    let base_oid = resolve_revision(repo, base).expect("base");
    let tip_oid = resolve_revision(repo, tip).expect("tip");
    let config = repo.config().expect("config").clone();
    let thread_policy = repo.thread_policy();
    let mut entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        &base_oid,
        &tip_oid,
        50,
        &config,
        &thread_policy,
    )
    .expect("diff");
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    let diff_opts = DiffOptions::default();
    let structured =
        tree_diff_structured_parallel(repo.odb(), entries, &config, &thread_policy, diff_opts)
            .expect("structured");
    let patch_opts = PatchTextOptions::from_repo(repo, diff_opts).expect("patch opts");
    let mut out = Cursor::new(Vec::new());
    write_patch_structured(repo, &structured, &patch_opts, &mut out)
        .expect("write_patch_structured");
    out.into_inner()
}

fn apply_patch_git_index(
    dir: &std::path::Path,
    base: &str,
    patch: &[u8],
    tip_ref: &str,
) -> ObjectId {
    git_in(dir, &["reset", "--hard", base]);
    std::fs::write(dir.join("patch.diff"), patch).expect("write patch");
    git_in(dir, &["apply", "--index", "patch.diff"]);
    let out = Command::new("git")
        .current_dir(dir)
        .args(["write-tree"])
        .output()
        .expect("write-tree");
    assert!(out.status.success());
    let tree = ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("tree");
    git_in(dir, &["reset", "--hard", tip_ref]);
    tree
}

fn assert_lib_apply_materializes_binary(repo: &Repository, base: &str, patch: &[u8], tip: &str) {
    let text = std::str::from_utf8(patch).expect("utf8 patch");
    let files = parse_patch(text, 1, "patch", false, None).expect("parse grit patch");
    let base_oid = resolve_revision(repo, base).expect("base");
    let tip_oid = resolve_revision(repo, tip).expect("tip");
    let tip_tree = parse_commit(&repo.odb().read(&tip_oid).expect("read tip").data)
        .expect("parse tip")
        .tree;
    for fp in files.iter().filter(|f| f.binary_patch.is_some()) {
        let path = fp
            .new_path
            .as_deref()
            .or(fp.old_path.as_deref())
            .expect("path");
        let base_tree = parse_commit(&repo.odb().read(&base_oid).expect("read base").data)
            .expect("parse base")
            .tree;
        let old_bytes = read_blob_at_path(repo.odb(), &base_tree, path).unwrap_or_default();
        let payload = fp.binary_patch.as_ref().expect("binary");
        let got = materialize_binary_patch_forward(payload, &old_bytes).expect("forward");
        let want = read_blob_at_path(repo.odb(), &tip_tree, path).expect("tip blob");
        assert_eq!(got, want, "binary patch forward for {path}");
    }
}

fn assert_patch_roundtrip(dir: &TempDir, base: &str, tip: &str, repo_tip: &str) {
    git_in(dir.path(), &["reset", "--hard", repo_tip]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default())
        .expect("open grit repo");
    let git_patch = git_diff_binary(dir.path(), base, tip);
    let grit_patch = grit_patch_structured(&repo, base, tip);
    if !git_patch.is_empty() && !git_patch.windows(17).any(|w| w == b"GIT binary patch\n") {
        assert_eq!(
            grit_patch, git_patch,
            "patch text must match git diff --binary -M50 for {base}..{tip}"
        );
    }
    if let Ok(text) = std::str::from_utf8(&grit_patch) {
        parse_patch(text, 1, "patch", false, None).expect("grit-lib apply parse");
    }
    if std::str::from_utf8(&grit_patch).is_ok() {
        assert_lib_apply_materializes_binary(&repo, base, &grit_patch, tip);
    }
    let expected = git_tree(dir.path(), tip);
    let via_git = apply_patch_git_index(dir.path(), base, &grit_patch, repo_tip);
    assert_eq!(via_git, expected, "git apply --index of grit patch");
}

fn build_fixture_repo(dir: &std::path::Path) {
    git_in(dir, &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.join("plain.txt"), b"one\n").unwrap();
    git_in(dir, &["add", "plain.txt"]);
    git_in(dir, &["commit", "-q", "-m", "init"]);

    std::fs::write(dir.join("plain.txt"), b"one\ntwo\n").unwrap();
    git_in(dir, &["add", "plain.txt"]);
    git_in(dir, &["commit", "-q", "-m", "edit"]);

    std::fs::write(dir.join("added.txt"), b"new\n").unwrap();
    git_in(dir, &["add", "added.txt"]);
    git_in(dir, &["commit", "-q", "-m", "add"]);

    git_in(dir, &["rm", "added.txt"]);
    git_in(dir, &["commit", "-q", "-m", "delete"]);

    std::fs::write(dir.join("rename-me.txt"), b"payload\n").unwrap();
    git_in(dir, &["add", "rename-me.txt"]);
    git_in(dir, &["commit", "-q", "-m", "before-rename"]);
    std::fs::rename(dir.join("rename-me.txt"), dir.join("renamed.txt")).unwrap();
    std::fs::write(dir.join("renamed.txt"), b"payload!\n").unwrap();
    git_in(dir, &["add", "-A"]);
    git_in(dir, &["commit", "-q", "-m", "rename"]);

    std::fs::write(dir.join("mode.txt"), b"x\n").unwrap();
    git_in(dir, &["add", "mode.txt"]);
    git_in(dir, &["commit", "-q", "-m", "mode-before"]);
    git_in(dir, &["update-index", "--chmod=+x", "mode.txt"]);
    git_in(dir, &["commit", "-qm", "mode-exec"]);

    std::os::unix::fs::symlink("target", dir.join("link")).unwrap();
    git_in(dir, &["add", "link"]);
    git_in(dir, &["commit", "-q", "-m", "symlink"]);
    std::fs::remove_file(dir.join("link")).unwrap();
    std::os::unix::fs::symlink("target2", dir.join("link")).unwrap();
    git_in(dir, &["add", "link"]);
    git_in(dir, &["commit", "-q", "-m", "symlink2"]);

    std::fs::write(dir.join("bin.dat"), &[0u8, 1, 2, 3, 255]).unwrap();
    git_in(dir, &["add", "bin.dat"]);
    git_in(dir, &["commit", "-q", "-m", "bin"]);
    std::fs::write(dir.join("bin.dat"), &[0u8, 1, 2, 4, 255]).unwrap();
    git_in(dir, &["add", "bin.dat"]);
    git_in(dir, &["commit", "-q", "-m", "bin2"]);

    std::fs::write(dir.join("no-nl.txt"), b"no newline").unwrap();
    git_in(dir, &["add", "no-nl.txt"]);
    git_in(dir, &["commit", "-q", "-m", "no-nl"]);
    std::fs::write(dir.join("no-nl.txt"), b"no newline!").unwrap();
    git_in(dir, &["add", "no-nl.txt"]);
    git_in(dir, &["commit", "-q", "-m", "no-nl2"]);

    std::fs::write(dir.join("crlf.txt"), b"a\r\nb\r\n").unwrap();
    git_in(dir, &["add", "crlf.txt"]);
    git_in(dir, &["commit", "-q", "-m", "crlf"]);
    std::fs::write(dir.join("crlf.txt"), b"a\r\nc\r\n").unwrap();
    git_in(dir, &["add", "crlf.txt"]);
    git_in(dir, &["commit", "-q", "-m", "crlf2"]);

    std::fs::write(dir.join("bad-utf8.txt"), b"before\xC0after").unwrap();
    git_in(dir, &["add", "bad-utf8.txt"]);
    git_in(dir, &["commit", "-q", "-m", "bad-utf8"]);
    std::fs::write(dir.join("bad-utf8.txt"), b"before\xC1after").unwrap();
    git_in(dir, &["add", "bad-utf8.txt"]);
    git_in(dir, &["commit", "-q", "-m", "bad-utf8-edit"]);
}

fn commit_oids_newest_first(dir: &std::path::Path) -> Vec<ObjectId> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-list", "HEAD"])
        .output()
        .expect("rev-list");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| ObjectId::from_hex(line.trim()).ok())
        .collect()
}

#[test]
fn patch_text_roundtrip_fixtures() {
    let dir = TempDir::new().expect("tempdir");
    build_fixture_repo(dir.path());
    let oids = commit_oids_newest_first(dir.path());
    assert!(oids.len() >= 2, "expected fixture commits");
    let tip_hex = oids[0].to_hex();
    for i in 0..oids.len().saturating_sub(1) {
        let base = oids[i + 1].to_hex();
        let tip = if i == 0 {
            tip_hex.clone()
        } else {
            oids[i].to_hex()
        };
        assert_patch_roundtrip(&dir, &base, &tip, &tip_hex);
    }
}
