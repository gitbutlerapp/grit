//! `write_patch` / `--patch-text` round-trip vs system `git diff --binary` and `git apply`.

use grit_lib::objects::FileMode;
use std::io::Cursor;
use std::process::Command;

use grit_lib::apply::{materialize_binary_patch_forward, parse_patch};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_diff::read_blob_at_path;
use grit_lib::diff::{
    diff_commits_with_renames_and_policy, tree_diff_structured_parallel, write_patch_resolved,
    write_patch_structured, DiffEntry, DiffOptions, DiffStatus, PatchResolvedSide, PatchSide,
    PatchTextOptions,
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
    let structured = tree_diff_structured_parallel(
        repo.odb(),
        entries,
        &config,
        &thread_policy,
        diff_opts.clone(),
    )
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

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_oid(dir: &std::path::Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(&git_out(dir, &["rev-parse", rev])).expect("oid")
}

fn entry(
    status: DiffStatus,
    path: &str,
    old: (&str, ObjectId),
    new: (&str, ObjectId),
) -> DiffEntry {
    DiffEntry {
        status,
        old_path: (old.0 != "000000").then(|| path.to_owned()),
        new_path: (new.0 != "000000").then(|| path.to_owned()),
        old_mode: FileMode::new(u32::from_str_radix(old.0, 8).expect("octal mode")),
        new_mode: FileMode::new(u32::from_str_radix(new.0, 8).expect("octal mode")),
        old_oid: old.1,
        new_oid: new.1,
        score: None,
    }
}

fn resolved_patch(repo: &Repository, files: &[PatchResolvedSide]) -> Vec<u8> {
    let opts = PatchTextOptions::from_repo(repo, DiffOptions::default()).expect("patch opts");
    let mut out = Cursor::new(Vec::new());
    write_patch_resolved(repo, files, &opts, &mut out).expect("write_patch_resolved");
    out.into_inner()
}

/// Tree `git apply --index` produces for `patch` in a fresh clone of `src` at `HEAD`.
fn git_apply_index_in_clone(src: &std::path::Path, patch: &[u8]) -> ObjectId {
    let scratch = TempDir::new().expect("tempdir");
    git_in(
        scratch.path(),
        &["clone", "-q", src.to_str().expect("utf8"), "dst"],
    );
    let dst = scratch.path().join("dst");
    std::fs::write(scratch.path().join("p.diff"), patch).expect("write patch");
    git_in(&dst, &["apply", "--index", "../p.diff"]);
    ObjectId::from_hex(&git_out(&dst, &["write-tree"])).expect("tree")
}

/// Repository with `b.bin`, `gone.bin` and `t.txt` committed at `HEAD`.
fn binary_fixture() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3]).unwrap();
    std::fs::write(dir.path().join("gone.bin"), [0u8, 9, 9]).unwrap();
    std::fs::write(dir.path().join("t.txt"), b"one\n").unwrap();
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

#[test]
fn write_patch_resolved_reads_worktree_binary_sides() {
    let dir = binary_fixture();
    let old_bin = git_oid(dir.path(), "HEAD:b.bin");
    let gone = git_oid(dir.path(), "HEAD:gone.bin");
    let old_txt = git_oid(dir.path(), "HEAD:t.txt");
    std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 4, 5, 6]).unwrap();
    std::fs::remove_file(dir.path().join("gone.bin")).unwrap();
    std::fs::write(dir.path().join("t.txt"), b"one\ntwo\n").unwrap();

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).expect("open");
    // Index-vs-worktree entries from the status fast path name the index blob on both sides.
    let files = vec![
        PatchResolvedSide {
            entry: entry(
                DiffStatus::Modified,
                "b.bin",
                ("100644", old_bin),
                ("100644", old_bin),
            ),
            old: PatchSide::Blob,
            new: PatchSide::Worktree,
        },
        PatchResolvedSide {
            entry: entry(
                DiffStatus::Deleted,
                "gone.bin",
                ("100644", gone),
                ("000000", ObjectId::zero()),
            ),
            old: PatchSide::Blob,
            new: PatchSide::Worktree,
        },
        PatchResolvedSide {
            entry: entry(
                DiffStatus::Modified,
                "t.txt",
                ("100644", old_txt),
                ("100644", old_txt),
            ),
            old: PatchSide::Bytes(b"one\n".to_vec()),
            new: PatchSide::Bytes(b"one\ntwo\n".to_vec()),
        },
    ];
    let patch = resolved_patch(&repo, &files);
    let text = String::from_utf8(patch.clone()).expect("utf8 patch");
    let worktree_bin =
        ObjectId::from_hex(&git_out(dir.path(), &["hash-object", "b.bin"])).expect("hash-object");
    assert!(
        text.contains(&format!(
            "index {}..{} 100644\nGIT binary patch\n",
            old_bin.to_hex(),
            worktree_bin.to_hex()
        )),
        "binary index line must name the worktree content:\n{text}"
    );
    assert!(text.contains(&format!(
        "index {}..{}\nGIT binary patch\n",
        gone.to_hex(),
        ObjectId::zero().to_hex()
    )));

    git_in(dir.path(), &["add", "-A"]);
    let want = ObjectId::from_hex(&git_out(dir.path(), &["write-tree"])).expect("tree");
    assert_eq!(git_apply_index_in_clone(dir.path(), &patch), want);
}

#[test]
fn write_patch_resolved_reads_staged_binary_blobs() {
    let dir = binary_fixture();
    let old_bin = git_oid(dir.path(), "HEAD:b.bin");
    std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 4, 5, 6]).unwrap();
    std::fs::write(dir.path().join("new.bin"), [0u8, 0, 7]).unwrap();
    git_in(dir.path(), &["add", "b.bin", "new.bin"]);
    let staged_bin = git_oid(dir.path(), ":b.bin");
    let staged_new = git_oid(dir.path(), ":new.bin");
    // The worktree moves on after staging; a staged patch must not read it.
    std::fs::write(dir.path().join("b.bin"), [0u8, 42]).unwrap();

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).expect("open");
    let files = vec![
        PatchResolvedSide {
            entry: entry(
                DiffStatus::Modified,
                "b.bin",
                ("100644", old_bin),
                ("100644", staged_bin),
            ),
            old: PatchSide::Blob,
            new: PatchSide::Blob,
        },
        PatchResolvedSide {
            entry: entry(
                DiffStatus::Added,
                "new.bin",
                ("000000", ObjectId::zero()),
                ("100644", staged_new),
            ),
            old: PatchSide::Blob,
            new: PatchSide::Blob,
        },
    ];
    let patch = resolved_patch(&repo, &files);
    let want = ObjectId::from_hex(&git_out(dir.path(), &["write-tree"])).expect("tree");
    assert_eq!(git_apply_index_in_clone(dir.path(), &patch), want);
}

#[test]
fn write_patch_resolved_hashes_in_memory_binary_bytes_for_index_line() {
    let dir = binary_fixture();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).expect("open");
    let old_bin = git_oid(dir.path(), "HEAD:b.bin");
    let new_bytes = vec![0u8, 1, 2, 4, 5, 6];
    let files = vec![PatchResolvedSide {
        entry: entry(
            DiffStatus::Modified,
            "b.bin",
            ("100644", old_bin),
            ("100644", old_bin),
        ),
        old: PatchSide::Bytes(vec![0u8, 1, 2, 3]),
        new: PatchSide::Bytes(new_bytes.clone()),
    }];
    let patch = resolved_patch(&repo, &files);
    std::fs::write(dir.path().join("b.bin"), &new_bytes).unwrap();
    git_in(dir.path(), &["add", "b.bin"]);
    let want = ObjectId::from_hex(&git_out(dir.path(), &["write-tree"])).expect("tree");
    assert_eq!(git_apply_index_in_clone(dir.path(), &patch), want);
}

#[test]
fn write_patch_resolved_worktree_side_needs_work_tree() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &grit_lib::repo::InitOptions::new().with_bare(true),
    )
    .expect("init bare");
    let files = vec![PatchResolvedSide {
        entry: entry(
            DiffStatus::Added,
            "b.bin",
            ("000000", ObjectId::zero()),
            ("100644", ObjectId::zero()),
        ),
        old: PatchSide::Blob,
        new: PatchSide::Worktree,
    }];
    let opts = PatchTextOptions::from_repo(&repo, DiffOptions::default()).expect("patch opts");
    let err = write_patch_resolved(&repo, &files, &opts, &mut std::io::sink())
        .expect_err("bare repository has no work tree");
    assert!(
        matches!(err, grit_lib::error::Error::RequiresWorkTree),
        "unexpected error: {err:?}"
    );
}

/// Issue #2244: `literal 0` must not appear when the blob is non-empty.
fn assert_binary_literals_nonzero(patch: &[u8], path: &str) {
    let text = std::str::from_utf8(patch).expect("utf8 patch");
    if !text.contains(&format!("diff --git a/{path} b/{path}")) {
        return;
    }
    let in_file = text
        .split(&format!("diff --git a/{path} b/{path}"))
        .nth(1)
        .unwrap_or(text);
    for line in in_file
        .lines()
        .take_while(|l| !l.starts_with("diff --git "))
    {
        if let Some(rest) = line.strip_prefix("literal ") {
            let size: usize = rest.parse().expect("literal size");
            assert!(size > 0, "literal 0 in binary patch for {path}:\n{text}");
        }
    }
}

fn git_apply_preimage_only(src: &std::path::Path, base: &str, rel: &str, patch: &[u8]) -> ObjectId {
    let scratch = TempDir::new().expect("tempdir");
    git_in(
        scratch.path(),
        &["init", "-q", "--initial-branch=main", "."],
    );
    let blob = Command::new("git")
        .current_dir(src)
        .args(["show", &format!("{base}:{rel}")])
        .output()
        .expect("git show blob");
    assert!(blob.status.success(), "git show {base}:{rel}");
    if let Some(parent) = scratch.path().join(rel).parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(scratch.path().join(rel), &blob.stdout).expect("write preimage");
    git_in(scratch.path(), &["add", rel]);
    std::fs::write(scratch.path().join("p.diff"), patch).expect("write patch");
    git_in(scratch.path(), &["apply", "--index", "p.diff"]);
    ObjectId::from_hex(&git_out(scratch.path(), &["write-tree"])).expect("tree")
}

#[test]
fn large_binary_commit_patch_applies_from_preimage_only() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    let mut v1 = vec![0u8; 100_000];
    for (i, b) in v1.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    v1[1000] = 0;
    std::fs::write(dir.path().join("x.bin"), &v1).unwrap();
    git_in(dir.path(), &["add", "x.bin"]);
    git_in(dir.path(), &["commit", "-q", "-m", "one"]);
    let mut v2 = v1.clone();
    v2[5000] = v2[5000].wrapping_add(42);
    std::fs::write(dir.path().join("x.bin"), &v2).unwrap();
    git_in(dir.path(), &["add", "x.bin"]);
    git_in(dir.path(), &["commit", "-q", "-m", "two"]);

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).expect("open");
    let patch = grit_patch_structured(&repo, "HEAD~1", "HEAD");
    assert_binary_literals_nonzero(&patch, "x.bin");
    let want = git_tree(dir.path(), "HEAD");
    assert_eq!(
        git_apply_preimage_only(dir.path(), "HEAD~1", "x.bin", &patch),
        want
    );
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
