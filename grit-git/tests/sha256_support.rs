//! End-to-end tests for SHA-256 (`--object-format=sha256`) repository support.
//!
//! Every supported subsystem is exercised and cross-checked against the system
//! `git` binary in both directions (grit writes → git verifies, and grit reads
//! git-written artifacts):
//!   - loose objects: init / commit / show / log / rev-list round-trips
//!   - reflog (files + reftable backends), null-OID width
//!   - diff `--raw` null/real OID widths (add / modify / delete)
//!   - abbreviated rev-parse, fast-import, notes, split index
//!   - packs: grit-written pack/idx/rev verified by git; grit reads git's packs
//!     including delta-compressed objects
//!   - `grit fsck` clean on loose and packed sha256 repos
//!   - commit-graph (hash-version 2) and multi-pack-index, git-verified
//!   - reftable (version 2 / 32-byte object ids): refs and reflog round-trips
//!   - clone / fetch / push over the wire protocol
//!
//! Fixtures that must be genuinely SHA-256-correct are built with the system
//! `git` binary; the behaviour under test is always grit's.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const GRIT_BIN: &str = env!("CARGO_BIN_EXE_grit-git");

/// A captured command result.
struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }
    fn dump(&self, label: &str) -> String {
        format!(
            "{label}: exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        )
    }
}

/// Create a fresh, uniquely-named temporary directory (no external deps).
fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("grit-sha256-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

/// Run a command in `dir` with a deterministic committer/author identity.
fn run(bin: &str, args: &[&str], dir: &Path) -> Output {
    let out = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {bin} {args:?}: {e}"));
    Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn grit(args: &[&str], dir: &Path) -> Output {
    run(GRIT_BIN, args, dir)
}

fn git(args: &[&str], dir: &Path) -> Output {
    run("git", args, dir)
}

/// Run grit feeding `stdin` to the child (for `fast-import` and similar).
fn grit_stdin(args: &[&str], dir: &Path, stdin: &[u8]) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(GRIT_BIN)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grit");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait grit");
    Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn is_hex64(s: &str) -> bool {
    let s = s.trim();
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Build a SHA-256 repository using the system `git`, with one or more commits.
/// Returns the repo path. Panics (rather than skips) if the system `git`
/// cannot create a sha256 repo, so the requirement is visible.
fn real_git_sha256_repo(tag: &str, files: &[(&str, &str, &str)]) -> PathBuf {
    let dir = unique_tmp(tag);
    let init = git(&["init", "--object-format=sha256", "-q", "."], &dir);
    assert!(
        init.ok(),
        "system `git init --object-format=sha256` failed — git >= 2.29 with sha256 is required for this test\n{}",
        init.dump("git init")
    );
    // Sanity: confirm the fixture really is sha256.
    let fmt = git(&["rev-parse", "--show-object-format"], &dir);
    assert_eq!(fmt.stdout.trim(), "sha256", "fixture is not sha256");

    for (name, contents, msg) in files {
        write_file(&dir, name, contents);
        assert!(git(&["add", name], &dir).ok(), "git add failed");
        let c = git(&["commit", "-q", "-m", msg], &dir);
        assert!(c.ok(), "{}", c.dump("git commit"));
    }
    dir
}

#[test]
fn sha256_init_and_commit_roundtrip_readable_by_git() {
    // init + commit driven entirely by grit; the resulting repo must be a
    // valid sha256 repo that the system git can read back.
    let dir = unique_tmp("init-commit");

    let init = grit(&["init", "--object-format=sha256", "."], &dir);
    assert!(init.ok(), "{}", init.dump("grit init"));

    let fmt = grit(&["rev-parse", "--show-object-format"], &dir);
    assert_eq!(
        fmt.stdout.trim(),
        "sha256",
        "grit did not record sha256 object format\n{}",
        fmt.dump("show-object-format")
    );

    write_file(&dir, "a.txt", "hello sha256\n");
    assert!(grit(&["add", "a.txt"], &dir).ok(), "grit add failed");
    let commit = grit(&["commit", "-m", "first"], &dir);
    assert!(commit.ok(), "{}", commit.dump("grit commit"));

    // The system git must be able to resolve HEAD as a 64-hex sha256 OID,
    // and the object store must pass fsck (i.e. grit wrote real sha256
    // objects, not sha1 ones).
    let head = git(&["rev-parse", "HEAD"], &dir);
    assert!(head.ok(), "{}", head.dump("git rev-parse HEAD"));
    assert!(
        is_hex64(&head.stdout),
        "HEAD is not a 64-char sha256 OID: {:?}",
        head.stdout.trim()
    );

    let fsck = git(&["fsck", "--strict"], &dir);
    assert!(
        fsck.ok(),
        "git fsck failed — grit corrupted the sha256 repo\n{}",
        fsck.dump("git fsck")
    );
}

#[test]
fn sha256_commit_produces_sha256_oid_resolvable_by_grit() {
    let dir = unique_tmp("commit-oid");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "grit init failed"
    );

    write_file(&dir, "f.txt", "content\n");
    assert!(grit(&["add", "f.txt"], &dir).ok(), "grit add failed");
    let commit = grit(&["commit", "-m", "msg"], &dir);
    assert!(commit.ok(), "{}", commit.dump("grit commit"));

    let head = grit(&["rev-parse", "HEAD"], &dir);
    assert!(head.ok(), "{}", head.dump("grit rev-parse HEAD"));
    assert!(
        is_hex64(&head.stdout),
        "grit rev-parse HEAD is not a 64-char sha256 OID: {:?}",
        head.stdout.trim()
    );
}

#[test]
fn sha256_show_reads_real_git_repo() {
    let dir = real_git_sha256_repo("show", &[("a.txt", "hello sha256\n", "first")]);

    let show = grit(&["show", "HEAD"], &dir);
    assert!(
        show.ok(),
        "grit show HEAD failed on a sha256 repo\n{}",
        show.dump("grit show")
    );
    assert!(
        show.stdout.contains("hello sha256"),
        "grit show output missing file contents\n{}",
        show.dump("grit show")
    );
}

#[test]
fn sha256_log_reads_real_git_repo() {
    // The originally reported bug: `grit log` -> "error: broken HEAD".
    let dir = real_git_sha256_repo("log", &[("a.txt", "x\n", "first commit")]);

    let log = grit(&["log"], &dir);
    assert!(
        !log.stderr.contains("broken HEAD"),
        "grit log reported 'broken HEAD' on a sha256 repo\n{}",
        log.dump("grit log")
    );
    assert!(
        log.ok(),
        "grit log failed on a sha256 repo\n{}",
        log.dump("grit log")
    );
    assert!(
        log.stdout.contains("first commit"),
        "grit log missing commit subject\n{}",
        log.dump("grit log")
    );
}

#[test]
fn sha256_rev_list_reads_real_git_repo() {
    let dir = real_git_sha256_repo(
        "rev-list",
        &[("a.txt", "one\n", "c1"), ("b.txt", "two\n", "c2")],
    );

    let rl = grit(&["rev-list", "HEAD"], &dir);
    assert!(
        rl.ok(),
        "grit rev-list HEAD failed on a sha256 repo\n{}",
        rl.dump("grit rev-list")
    );
    let lines: Vec<&str> = rl.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected 2 commits from rev-list\n{}",
        rl.dump("grit rev-list")
    );
    for line in lines {
        assert!(
            is_hex64(line),
            "rev-list emitted a non-sha256 OID: {line:?}\n{}",
            rl.dump("grit rev-list")
        );
    }
}

#[test]
fn sha256_repack_pack_verified_by_git() {
    // grit must be able to write a sha256 pack + idx + rev that the system git
    // accepts (verify-pack + fsck), and then read its own pack back.
    let dir = unique_tmp("repack");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "grit init failed"
    );
    for i in 1..=3 {
        write_file(&dir, &format!("f{i}.txt"), &format!("content {i}\n"));
        assert!(
            grit(&["add", &format!("f{i}.txt")], &dir).ok(),
            "grit add failed"
        );
        assert!(
            grit(&["commit", "-m", &format!("c{i}")], &dir).ok(),
            "grit commit failed"
        );
    }

    let repack = grit(&["repack", "-a", "-d"], &dir);
    assert!(repack.ok(), "{}", repack.dump("grit repack"));

    // The system git must accept grit's sha256 pack.
    let idx_glob = std::fs::read_dir(dir.join(".git/objects/pack"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("no .idx written by repack");
    let verify = git(&["verify-pack", "-v", idx_glob.to_str().unwrap()], &dir);
    assert!(
        verify.ok(),
        "git verify-pack rejected grit's sha256 pack\n{}",
        verify.dump("verify-pack")
    );
    let fsck = git(&["fsck", "--strict"], &dir);
    assert!(
        fsck.ok(),
        "git fsck failed on grit's sha256 pack\n{}",
        fsck.dump("git fsck")
    );

    // grit reads its own pack (loose objects were pruned by -d).
    let log = grit(&["log", "--oneline"], &dir);
    assert!(log.ok(), "{}", log.dump("grit log after repack"));
    assert_eq!(
        log.stdout.lines().count(),
        3,
        "grit log lost commits after repack"
    );
}

#[test]
fn sha256_reads_git_written_pack() {
    // grit must read a pack produced by the system git in a sha256 repo.
    let dir = real_git_sha256_repo(
        "readpack",
        &[("a.txt", "x\n", "c1"), ("b.txt", "y\n", "c2")],
    );
    let repack = git(&["repack", "-a", "-d", "-q"], &dir);
    assert!(repack.ok(), "{}", repack.dump("git repack"));

    let log = grit(&["log", "--oneline"], &dir);
    assert!(
        log.ok(),
        "grit could not read git's sha256 pack\n{}",
        log.dump("grit log")
    );
    assert_eq!(
        log.stdout.lines().count(),
        2,
        "grit lost commits reading git's pack"
    );

    let fsck = grit(&["fsck"], &dir);
    assert!(
        fsck.ok(),
        "grit fsck failed on git's sha256 pack\n{}",
        fsck.dump("grit fsck")
    );
}

#[test]
fn sha256_reftable_refs_roundtrip() {
    // A sha256 repo using the reftable backend must round-trip refs through both
    // grit and the system git (reftable version 2 / 32-byte object ids).
    let dir = unique_tmp("reftable");
    let init = grit(
        &[
            "init",
            "--object-format=sha256",
            "--ref-format=reftable",
            "-b",
            "main",
            ".",
        ],
        &dir,
    );
    assert!(init.ok(), "{}", init.dump("grit init reftable sha256"));

    write_file(&dir, "a.txt", "one\n");
    assert!(grit(&["add", "a.txt"], &dir).ok(), "add");
    assert!(grit(&["commit", "-m", "c1"], &dir).ok(), "commit");
    assert!(grit(&["branch", "feature"], &dir).ok(), "branch");

    // grit reads its own reftable.
    let refs = grit(&["for-each-ref", "--format=%(refname)"], &dir);
    assert!(refs.ok(), "{}", refs.dump("grit for-each-ref"));
    assert!(
        refs.stdout.contains("refs/heads/main"),
        "missing main\n{}",
        refs.dump("for-each-ref")
    );
    assert!(
        refs.stdout.contains("refs/heads/feature"),
        "missing feature\n{}",
        refs.dump("for-each-ref")
    );

    // The system git must read grit's sha256 reftable (version 2) when built with
    // reftable support (Git 2.45+). Older system git binaries skip this leg.
    let gshow = git(&["show-ref"], &dir);
    if !gshow.ok() && gshow.stderr.contains("refstorage") {
        return;
    }
    assert!(
        gshow.ok(),
        "git could not read grit's sha256 reftable\n{}",
        gshow.dump("git show-ref")
    );
    assert!(
        gshow.stdout.contains("refs/heads/main"),
        "git missing main\n{}",
        gshow.dump("git show-ref")
    );
    // Each ref line is a 64-hex sha256 OID.
    for line in gshow.stdout.lines() {
        let oid = line.split_whitespace().next().unwrap_or("");
        assert_eq!(oid.len(), 64, "reftable ref oid not sha256-width: {line:?}");
    }
}

#[test]
fn sha256_commit_graph_write_and_read() {
    // grit must write a commit-graph (hash version 2) that the system git
    // accepts, and read it back when resolving history.
    let dir = unique_tmp("cgraph");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "init"
    );
    for i in 1..=4 {
        write_file(&dir, "f.txt", &format!("c{i}\n"));
        assert!(grit(&["add", "f.txt"], &dir).ok(), "add");
        assert!(
            grit(&["commit", "-m", &format!("c{i}")], &dir).ok(),
            "commit"
        );
    }
    let write = grit(&["commit-graph", "write"], &dir);
    assert!(write.ok(), "{}", write.dump("grit commit-graph write"));
    let gverify = git(&["commit-graph", "verify"], &dir);
    assert!(
        gverify.ok(),
        "git rejected grit's sha256 commit-graph\n{}",
        gverify.dump("git commit-graph verify")
    );
    // grit reads its own commit-graph for history.
    let log = grit(&["log", "--oneline"], &dir);
    assert!(
        log.ok() && log.stdout.lines().count() == 4,
        "{}",
        log.dump("grit log w/ commit-graph")
    );
}

#[test]
fn sha256_multi_pack_index_write_and_read() {
    // grit must write a multi-pack-index that git accepts and read objects via it.
    let dir = unique_tmp("midx");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "init"
    );
    // Build two packs deterministically (bitmaps disabled — a separate feature)
    // so the MIDX covers multiple packs.
    for i in 1..=2 {
        write_file(&dir, &format!("f{i}.txt"), &format!("c{i}\n"));
        assert!(grit(&["add", &format!("f{i}.txt")], &dir).ok(), "add");
        assert!(
            grit(&["commit", "-m", &format!("c{i}")], &dir).ok(),
            "commit"
        );
        assert!(
            grit(&["-c", "repack.writeBitmaps=false", "repack"], &dir).ok(),
            "repack"
        );
    }
    // Write a v1 MIDX (the format the system git verifies across versions).
    let write = grit(&["-c", "midx.version=1", "multi-pack-index", "write"], &dir);
    assert!(write.ok(), "{}", write.dump("grit multi-pack-index write"));
    let gverify = git(&["multi-pack-index", "verify"], &dir);
    assert!(
        gverify.ok(),
        "git rejected grit's sha256 MIDX\n{}",
        gverify.dump("git midx verify")
    );
    let grverify = grit(&["multi-pack-index", "verify"], &dir);
    assert!(
        grverify.ok(),
        "grit could not verify its own sha256 MIDX\n{}",
        grverify.dump("grit midx verify")
    );
    // Read an object through the MIDX.
    let head = grit(&["rev-parse", "HEAD"], &dir);
    let t = grit(
        &[
            "-c",
            "core.multiPackIndex=true",
            "cat-file",
            "-t",
            head.stdout.trim(),
        ],
        &dir,
    );
    assert!(
        t.ok() && t.stdout.trim() == "commit",
        "{}",
        t.dump("grit cat-file via midx")
    );
}

#[test]
fn sha256_clone_fetch_push_roundtrip() {
    // Exercise the wire protocol (clone, push, fetch) end-to-end on sha256:
    // the pack (un)packing, object-format negotiation, and ref handling.
    let src = unique_tmp("net-src");
    assert!(
        grit(&["init", "--object-format=sha256", "-b", "main", "."], &src,).ok(),
        "init src"
    );
    write_file(&src, "a.txt", "one\n");
    assert!(grit(&["add", "a.txt"], &src).ok(), "add");
    assert!(grit(&["commit", "-m", "c1"], &src).ok(), "commit");

    // clone (sha256 must propagate)
    let dst = unique_tmp("net-dst");
    let clone = grit(
        &["clone", src.to_str().unwrap(), dst.to_str().unwrap()],
        &src,
    );
    assert!(clone.ok(), "{}", clone.dump("grit clone"));
    let fmt = grit(&["rev-parse", "--show-object-format"], &dst);
    assert_eq!(
        fmt.stdout.trim(),
        "sha256",
        "clone did not propagate sha256"
    );

    // push a new commit from the clone to a bare sha256 remote
    let bare = unique_tmp("net-bare");
    assert!(
        git(
            &["init", "--bare", "--object-format=sha256", "-q", "."],
            &bare
        )
        .ok(),
        "init bare"
    );
    write_file(&dst, "b.txt", "two\n");
    assert!(grit(&["add", "b.txt"], &dst).ok(), "add b");
    assert!(grit(&["commit", "-m", "c2"], &dst).ok(), "commit c2");
    let push = grit(&["push", bare.to_str().unwrap(), "main"], &dst);
    assert!(push.ok(), "{}", push.dump("grit push"));
    // system git must accept the pushed pack
    let gfsck = git(&["fsck"], &bare);
    assert!(
        gfsck.ok(),
        "git fsck on pushed sha256 repo\n{}",
        gfsck.dump("git fsck")
    );

    // fetch the bare back into the original src
    assert!(
        grit(&["remote", "add", "bare", bare.to_str().unwrap()], &src).ok(),
        "remote add"
    );
    let fetch = grit(&["fetch", "bare"], &src);
    assert!(fetch.ok(), "{}", fetch.dump("grit fetch"));
    let log = grit(&["log", "--oneline", "bare/main"], &src);
    assert!(log.ok(), "{}", log.dump("grit log bare/main"));
    assert_eq!(log.stdout.lines().count(), 2, "fetched history wrong size");
}

#[test]
fn sha256_reflog_records_64_hex_oids() {
    // The reflog parser hardcoded SHA-1 (40-hex) line geometry and silently
    // dropped every sha256 entry. After two grit commits, `grit reflog` must
    // show both, with 64-hex OIDs.
    let dir = unique_tmp("reflog");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "grit init failed"
    );
    write_file(&dir, "a.txt", "one\n");
    assert!(grit(&["add", "a.txt"], &dir).ok(), "grit add a failed");
    assert!(
        grit(&["commit", "-m", "c1"], &dir).ok(),
        "grit commit c1 failed"
    );
    write_file(&dir, "a.txt", "two\n");
    assert!(grit(&["add", "a.txt"], &dir).ok(), "grit add b failed");
    assert!(
        grit(&["commit", "-m", "c2"], &dir).ok(),
        "grit commit c2 failed"
    );

    let rl = grit(&["reflog"], &dir);
    assert!(rl.ok(), "{}", rl.dump("grit reflog"));
    let lines: Vec<&str> = rl.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected 2 reflog entries (sha256 lines must not be dropped)\n{}",
        rl.dump("grit reflog")
    );
    // Each `grit reflog` line begins with the abbreviated new OID; verify the
    // underlying logs/HEAD carries full 64-hex OIDs.
    let head_log = std::fs::read_to_string(dir.join(".git/logs/HEAD")).unwrap_or_default();
    for line in head_log.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(3, ' ');
        let old = parts.next().unwrap_or("");
        let new = parts.next().unwrap_or("");
        assert!(
            old.len() == 64 && new.len() == 64,
            "logs/HEAD entry is not sha256-width: {line:?}"
        );
    }
}

#[test]
fn sha256_diff_raw_null_oid_is_64_zeros() {
    // `git diff --raw` prints null OIDs at the repository's hash width. For an
    // added file the old OID side must be 64 zeros in a sha256 repo, not 40.
    let dir = unique_tmp("diff-raw");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "grit init failed"
    );
    write_file(&dir, "base.txt", "base\n");
    assert!(
        grit(&["add", "base.txt"], &dir).ok(),
        "grit add base failed"
    );
    assert!(
        grit(&["commit", "-m", "base"], &dir).ok(),
        "grit commit base failed"
    );

    write_file(&dir, "added.txt", "new\n");
    assert!(
        grit(&["add", "added.txt"], &dir).ok(),
        "grit add added failed"
    );

    let diff = grit(&["diff", "--cached", "--raw", "--no-abbrev"], &dir);
    assert!(diff.ok(), "{}", diff.dump("grit diff --raw"));
    let line = diff
        .stdout
        .lines()
        .find(|l| l.contains("added.txt"))
        .unwrap_or_else(|| {
            panic!(
                "no raw line for added.txt\n{}",
                diff.dump("grit diff --raw")
            )
        });
    // Format: :<old-mode> <new-mode> <old-oid> <new-oid> A\tadded.txt
    let fields: Vec<&str> = line.trim_start_matches(':').split_whitespace().collect();
    let old_oid = fields.get(2).copied().unwrap_or("");
    let new_oid = fields.get(3).copied().unwrap_or("");
    assert_eq!(
        old_oid,
        "0".repeat(64),
        "raw diff old (null) OID is not 64 zeros in a sha256 repo: {line:?}"
    );
    assert_eq!(
        new_oid.len(),
        64,
        "raw diff new OID is not full sha256 width: {line:?}"
    );
}

#[test]
fn sha256_abbreviated_rev_parse_resolves() {
    // Abbreviated OID resolution thresholds were hardcoded to 40; a sha256
    // prefix longer than 40 chars must still resolve as an abbreviation.
    let dir = real_git_sha256_repo("abbrev", &[("a.txt", "hello\n", "first")]);
    let head = grit(&["rev-parse", "HEAD"], &dir);
    assert!(head.ok(), "{}", head.dump("grit rev-parse HEAD"));
    let full = head.stdout.trim().to_string();
    assert_eq!(full.len(), 64, "HEAD not 64 hex: {full:?}");

    // A 50-char prefix (>40, <64) is still an abbreviation and must resolve.
    let prefix = &full[..50];
    let resolved = grit(&["rev-parse", prefix], &dir);
    assert!(
        resolved.ok(),
        "grit rev-parse of a 50-char sha256 prefix failed\n{}",
        resolved.dump("grit rev-parse prefix")
    );
    assert_eq!(
        resolved.stdout.trim(),
        full,
        "abbreviated sha256 OID did not resolve to the full OID"
    );
}

#[test]
fn sha256_fsck_clean_loose_and_packed() {
    // grit's fsck object/ref/pack checks were SHA-1-width-hardcoded. A sha256
    // repo must pass `grit fsck` both with loose objects and after a repack.
    let dir = unique_tmp("fsck");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "init"
    );
    for i in 1..=3 {
        write_file(&dir, "f.txt", &format!("c{i}\n"));
        assert!(grit(&["add", "f.txt"], &dir).ok(), "add");
        assert!(
            grit(&["commit", "-m", &format!("c{i}")], &dir).ok(),
            "commit"
        );
    }
    let loose = grit(&["fsck"], &dir);
    assert!(
        loose.ok(),
        "grit fsck failed on loose sha256 repo\n{}",
        loose.dump("grit fsck loose")
    );
    assert!(
        !loose.stderr.contains("badTreeSha1") && !loose.stderr.contains("badRefContent"),
        "grit fsck reported sha256-width errors (loose)\n{}",
        loose.dump("grit fsck loose")
    );

    assert!(grit(&["repack", "-a", "-d"], &dir).ok(), "repack");
    let packed = grit(&["fsck"], &dir);
    assert!(
        packed.ok(),
        "grit fsck failed on packed sha256 repo\n{}",
        packed.dump("grit fsck packed")
    );
    assert!(
        !packed.stderr.contains("does not match")
            && !packed.stderr.contains("invalid oid")
            && !packed.stderr.contains("missing object 000"),
        "grit fsck reported sha256-width errors (packed)\n{}",
        packed.dump("grit fsck packed")
    );
}

#[test]
fn sha256_split_index_roundtrip() {
    // The split-index body hash and trailer were SHA-1-only; in a sha256 repo
    // the shared index must be written and read back consistently.
    let dir = unique_tmp("split-index");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "init"
    );
    assert!(
        grit(&["config", "core.splitIndex", "true"], &dir).ok(),
        "config"
    );
    write_file(&dir, "a.txt", "a\n");
    assert!(grit(&["add", "a.txt"], &dir).ok(), "add a");
    assert!(
        grit(&["update-index", "--split-index"], &dir).ok(),
        "split-index"
    );
    write_file(&dir, "b.txt", "b\n");
    assert!(grit(&["add", "b.txt"], &dir).ok(), "add b");
    assert!(grit(&["commit", "-m", "c1"], &dir).ok(), "commit");

    // A 64-hex shared index name must exist and grit must read the merged set.
    let shared = std::fs::read_dir(dir.join(".git"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix("sharedindex.").is_some_and(is_hex64)
        });
    assert!(
        shared,
        "no 64-hex shared index written for sha256 split index"
    );
    let files = grit(&["ls-files"], &dir);
    assert!(files.ok(), "{}", files.dump("grit ls-files"));
    assert!(
        files.stdout.contains("a.txt") && files.stdout.contains("b.txt"),
        "split index lost entries\n{}",
        files.dump("grit ls-files")
    );
    // System git must also read the split index.
    let gfiles = git(&["ls-files"], &dir);
    assert!(
        gfiles.ok() && gfiles.stdout.contains("a.txt"),
        "git could not read sha256 split index\n{}",
        gfiles.dump("git ls-files")
    );
}

#[test]
fn sha256_fast_import_creates_sha256_objects() {
    // fast-import hex-ref parsing was 40-char-only; importing into a sha256 repo
    // must produce a 64-hex commit resolvable by grit.
    let dir = unique_tmp("fast-import");
    assert!(
        grit(&["init", "--object-format=sha256", "-b", "main", "."], &dir,).ok(),
        "init"
    );
    let stream = b"blob\nmark :1\ndata 6\nhello\n\ncommit refs/heads/main\nmark :2\ncommitter Test <test@example.com> 1700000000 +0000\ndata 2\nc1\nM 100644 :1 a.txt\n";
    let imp = grit_stdin(&["fast-import"], &dir, stream);
    assert!(imp.ok(), "{}", imp.dump("grit fast-import"));
    let head = grit(&["rev-parse", "HEAD"], &dir);
    assert!(
        head.ok() && is_hex64(&head.stdout),
        "fast-import HEAD not sha256\n{}",
        head.dump("rev-parse HEAD")
    );
    let show = grit(&["cat-file", "-p", "HEAD:a.txt"], &dir);
    assert!(
        show.ok() && show.stdout.contains("hello"),
        "{}",
        show.dump("cat-file blob")
    );
}

#[test]
fn sha256_notes_resolve() {
    // note_object_name parsed a 40-char path; notes must work in a sha256 repo.
    let dir = unique_tmp("notes");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "init"
    );
    write_file(&dir, "a.txt", "x\n");
    assert!(grit(&["add", "a.txt"], &dir).ok(), "add");
    assert!(grit(&["commit", "-m", "c1"], &dir).ok(), "commit");
    assert!(
        grit(&["notes", "add", "-m", "a note", "HEAD"], &dir).ok(),
        "notes add"
    );
    let show = grit(&["notes", "show", "HEAD"], &dir);
    assert!(
        show.ok() && show.stdout.contains("a note"),
        "{}",
        show.dump("grit notes show")
    );
}

#[test]
fn sha256_grit_reads_git_delta_pack() {
    // grit's packed-object/delta read path was gated on 20-byte OIDs. grit must
    // resolve an object stored as a delta in a git-written sha256 pack.
    let dir = unique_tmp("delta");
    assert!(
        git(&["init", "--object-format=sha256", "-q", "."], &dir).ok(),
        "git init"
    );
    // A large file with small per-commit edits deltifies well.
    let base: String = (0..2000)
        .map(|i| format!("line {i} content padding padding\n"))
        .collect();
    write_file(&dir, "big.txt", &base);
    assert!(git(&["add", "big.txt"], &dir).ok(), "add");
    assert!(
        git(
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-q",
                "-m",
                "base"
            ],
            &dir
        )
        .ok(),
        "commit base"
    );
    for i in 1..=5 {
        let edited = base.replacen("line 0 ", &format!("CHANGED{i} "), 1);
        write_file(&dir, "big.txt", &edited);
        assert!(git(&["add", "big.txt"], &dir).ok(), "add edit");
        assert!(
            git(
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@e",
                    "commit",
                    "-q",
                    "-m",
                    &format!("e{i}")
                ],
                &dir
            )
            .ok(),
            "commit edit"
        );
    }
    let repack = git(&["repack", "-a", "-d", "-f", "--window=50", "-q"], &dir);
    assert!(repack.ok(), "{}", repack.dump("git repack"));
    // Confirm the pack actually contains deltas (otherwise the test is vacuous).
    let idx = std::fs::read_dir(dir.join(".git/objects/pack"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .unwrap();
    let vp = git(&["verify-pack", "-v", idx.to_str().unwrap()], &dir);
    assert!(
        vp.stdout.contains("chain length ="),
        "test fixture has no delta objects — not exercising delta read\n{}",
        vp.dump("verify-pack")
    );

    // grit reads the latest delta-compressed blob and passes fsck.
    let cat = grit(&["cat-file", "-p", "HEAD:big.txt"], &dir);
    assert!(
        cat.ok(),
        "grit could not read delta-compressed sha256 blob\n{}",
        cat.dump("grit cat-file")
    );
    assert!(
        cat.stdout.contains("CHANGED5"),
        "grit read stale/wrong blob from delta pack"
    );
    let fsck = grit(&["fsck"], &dir);
    assert!(
        fsck.ok(),
        "grit fsck failed on git's delta sha256 pack\n{}",
        fsck.dump("grit fsck")
    );
}

#[test]
fn sha256_reftable_reflog_roundtrip() {
    // The reftable v2 footer offsets and log-record widths were SHA-1-sized;
    // reflog entries in a sha256 reftable repo must read back via grit.
    let dir = unique_tmp("reftable-reflog");
    assert!(
        grit(
            &[
                "init",
                "--object-format=sha256",
                "--ref-format=reftable",
                "."
            ],
            &dir
        )
        .ok(),
        "init"
    );
    for i in 1..=2 {
        write_file(&dir, "f.txt", &format!("c{i}\n"));
        assert!(grit(&["add", "f.txt"], &dir).ok(), "add");
        assert!(
            grit(&["commit", "-m", &format!("c{i}")], &dir).ok(),
            "commit"
        );
    }
    let reflog = grit(&["reflog"], &dir);
    assert!(
        reflog.ok(),
        "{}",
        reflog.dump("grit reflog (reftable sha256)")
    );
    let lines: Vec<&str> = reflog
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "sha256 reftable reflog did not round-trip both entries\n{}",
        reflog.dump("grit reflog")
    );
    assert!(
        lines[0].contains("c2") && lines[1].contains("c1"),
        "reflog entries out of order/missing\n{}",
        reflog.dump("grit reflog")
    );
}

#[test]
fn sha256_diff_raw_modify_and_delete() {
    // Beyond the added-file case: a modified file shows two real 64-hex OIDs and
    // a deleted file shows a 64-zero new OID in `--raw --no-abbrev` output.
    let dir = unique_tmp("diff-md");
    assert!(
        grit(&["init", "--object-format=sha256", "."], &dir).ok(),
        "init"
    );
    write_file(&dir, "keep.txt", "v1\n");
    write_file(&dir, "gone.txt", "bye\n");
    assert!(grit(&["add", "keep.txt", "gone.txt"], &dir).ok(), "add");
    assert!(grit(&["commit", "-m", "c1"], &dir).ok(), "commit");
    write_file(&dir, "keep.txt", "v2\n");
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    assert!(grit(&["add", "-A"], &dir).ok(), "add -A");

    let diff = grit(&["diff", "--cached", "--raw", "--no-abbrev"], &dir);
    assert!(diff.ok(), "{}", diff.dump("grit diff --raw"));
    let modline = diff
        .stdout
        .lines()
        .find(|l| l.contains("keep.txt"))
        .unwrap_or("");
    let mf: Vec<&str> = modline.trim_start_matches(':').split_whitespace().collect();
    assert_eq!(
        mf.get(2).map(|s| s.len()),
        Some(64),
        "modified old oid not 64 hex: {modline:?}"
    );
    assert_eq!(
        mf.get(3).map(|s| s.len()),
        Some(64),
        "modified new oid not 64 hex: {modline:?}"
    );

    let delline = diff
        .stdout
        .lines()
        .find(|l| l.contains("gone.txt"))
        .unwrap_or("");
    let df: Vec<&str> = delline.trim_start_matches(':').split_whitespace().collect();
    assert_eq!(
        df.get(3),
        Some(&"0".repeat(64).as_str()),
        "deleted new oid not 64 zeros: {delline:?}"
    );
    assert_eq!(
        df.get(2).map(|s| s.len()),
        Some(64),
        "deleted old oid not 64 hex: {delline:?}"
    );
}
