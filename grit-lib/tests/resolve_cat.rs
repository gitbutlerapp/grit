//! `resolve` / `read_object_for_display` compatibility with system `git`.

use crate::support;

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::config::environment::Environment;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::object_inspect::{format_tree_cat_file_p, ObjectContents, ResolveError};
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

use support::{
    bare_repo_with_ambiguous_prefix, commit_file, git, git_rev_parse_verify, open_grit_repo,
    HistoryRepo,
};

const ABSENT_GITLINK: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

fn git_cat_file_type(wt: &std::path::Path, spec: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(wt)
        .args(["cat-file", "-t", spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_cat_file_size(wt: &std::path::Path, spec: &str) -> Option<u64> {
    let out = std::process::Command::new("git")
        .current_dir(wt)
        .args(["cat-file", "-s", spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    std::str::from_utf8(&out.stdout).ok()?.trim().parse().ok()
}

fn git_cat_file_p(wt: &std::path::Path, spec: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("git")
        .current_dir(wt)
        .args(["cat-file", "-p", spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

fn setup_rich_fixture() -> HistoryRepo {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "README.md", "# hello\n", "initial");
    git(wt, &["tag", "v0.1"]);
    git(wt, &["tag", "-a", "v1.0", "-m", "release one"]);
    git(wt, &["checkout", "-b", "feature"]);
    commit_file(wt, "feature.txt", "feat\n", "on feature");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "main.txt", "main\n", "on main");
    git(
        wt,
        &["merge", "--no-edit", "feature", "-m", "merge feature"],
    );
    commit_file(wt, "after-merge.txt", "done\n", "after merge");
    commit_file(wt, "pack-me.txt", "pack\n", "before repack");
    git(wt, &["repack", "-a", "-d"]);
    hist
}

#[test]
fn resolve_matches_git_rev_parse() {
    let hist = setup_rich_fixture();
    let wt = hist.worktree();
    let repo = open_grit_repo(wt);

    let specs = [
        "HEAD",
        "HEAD~1",
        "HEAD~2",
        "HEAD~3",
        "HEAD~3",
        "HEAD^",
        "HEAD^1",
        "HEAD~2^2",
        "main",
        "feature",
        "v0.1",
        "v1.0",
        "v1.0^{commit}",
        "v1.0^{tag}",
        "v1.0^{}",
        "HEAD^{tree}",
        "HEAD^{commit}",
        "HEAD:README.md",
        "HEAD:feature.txt",
        "main:after-merge.txt",
        "HEAD~1^{tree}",
        "refs/heads/main",
        "refs/tags/v1.0",
        "main~1",
        "main~2",
        "feature~0",
        "@{0}",
        "HEAD~0",
        "main^{commit}",
        "HEAD^{object}",
        "HEAD~4",
        "HEAD~1^{commit}",
        "feature^{}",
        "v0.1^{commit}",
        "HEAD:main.txt",
        "HEAD~2:README.md",
    ];

    for spec in specs {
        let git_oid =
            git_rev_parse_verify(wt, spec).unwrap_or_else(|| panic!("git rev-parse {spec}"));
        let resolved = repo
            .resolve(spec)
            .unwrap_or_else(|e| panic!("grit resolve {spec}: {e}"));
        assert_eq!(resolved.oid, git_oid, "oid mismatch for `{spec}`");
        let git_kind = git_cat_file_type(wt, &git_oid.to_hex())
            .unwrap_or_else(|| panic!("git cat-file -t {}", git_oid.to_hex()));
        assert_eq!(
            kind_str(resolved.kind),
            git_kind.as_str(),
            "kind mismatch for `{spec}`"
        );
    }
}

#[test]
fn cat_matches_git_cat_file() {
    let hist = setup_rich_fixture();
    let wt = hist.worktree();
    let repo = open_grit_repo(wt);

    let specs = [
        "HEAD",
        "HEAD^{tree}",
        "HEAD:README.md",
        "v1.0^{tag}",
        "v1.0^{commit}",
        "HEAD~2",
        "feature^{commit}",
    ];

    for spec in specs {
        let resolved = repo.resolve(spec).expect("resolve");
        let git_type = git_cat_file_type(wt, spec).expect("git -t");
        assert_eq!(kind_str(resolved.kind), git_type.as_str());
        let git_size = git_cat_file_size(wt, spec).expect("git -s");
        assert_eq!(resolved.size, git_size, "size for {spec}");

        let git_body = git_cat_file_p(wt, spec).expect("git -p");
        let obj = repo
            .read_object_raw_for_display(&resolved.oid)
            .expect("read object");
        let _parsed = repo
            .read_object_for_display(&resolved.oid)
            .expect("read_object_for_display");

        match git_type.as_str() {
            "blob" => {
                assert!(matches!(_parsed, ObjectContents::Blob(_)));
                assert_eq!(obj.data, git_body, "blob body for {spec}");
            }
            "tree" => {
                assert!(matches!(_parsed, ObjectContents::Tree(_)));
                assert_eq!(resolved.kind, ObjectKind::Tree);
                let grit_body = format_tree_cat_file_p(&obj.data).expect("format tree");
                assert_eq!(grit_body, git_body, "tree cat-file -p body for {spec}");
            }
            "commit" => {
                assert!(matches!(_parsed, ObjectContents::Commit(_)));
                assert_eq!(obj.data, git_body, "commit body for {spec}");
            }
            "tag" => {
                assert!(matches!(_parsed, ObjectContents::Tag(_)));
                assert_eq!(obj.data, git_body, "tag body for {spec}");
            }
            other => panic!("unexpected kind {other} for {spec}"),
        }
    }
}

#[test]
fn cat_commit_non_utf8_payload_matches_git_cat_file_p() {
    use grit_lib::objects::ObjectId;
    use grit_test_support::{git_hash_object_literally, HashAlgo};

    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "base.txt", "x\n", "base");
    let tree = git(wt, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let mut body =
        format!("tree {tree}\nauthor Test <t@test> 0 +0000\ncommitter Test <t@test> 0 +0000\n\n")
            .into_bytes();
    body.push(0xff);
    body.extend_from_slice(b"note\n");
    let oid_hex = git_hash_object_literally(wt, HashAlgo::Sha1, "commit", &body)
        .expect("git hash-object --literally");
    let git_body = git_cat_file_p(wt, &oid_hex).expect("git -p");
    let repo = open_grit_repo(wt);
    let oid = ObjectId::from_hex(&oid_hex).expect("hex");
    let obj = repo.read_object_raw_for_display(&oid).expect("read");
    assert_eq!(obj.data, git_body, "commit payload bytes");
    assert_eq!(
        obj.data.len(),
        git_body.len(),
        "byte length vs git cat-file -p"
    );
    assert!(
        !std::str::from_utf8(&obj.data).is_ok(),
        "fixture should contain non-UTF-8"
    );
}

#[test]
fn cat_tree_missing_gitlink_matches_git_cat_file_p() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "x\n", "init");
    let tree_oid = {
        let mut child = Command::new("git")
            .current_dir(wt)
            .args(["mktree"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("mktree");
        writeln!(
            child.stdin.as_mut().expect("stdin"),
            "160000 commit {ABSENT_GITLINK}\tsubmodule"
        )
        .unwrap();
        let out = child.wait_with_output().expect("mktree wait");
        assert!(
            out.status.success(),
            "mktree: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let spec = tree_oid.as_str();
    let git_body = git_cat_file_p(wt, spec).expect("git -p");
    assert!(
        git_body
            .split(|&b| b == b'\n')
            .any(|line| line.starts_with(b"160000 commit ")),
        "git should label gitlink as commit: {}",
        String::from_utf8_lossy(&git_body)
    );
    let repo = open_grit_repo(wt);
    let oid = grit_lib::objects::ObjectId::from_hex(&tree_oid).expect("hex");
    let obj = repo.read_object_raw_for_display(&oid).expect("read tree");
    let grit_body = format_tree_cat_file_p(&obj.data).expect("format tree");
    assert_eq!(grit_body, git_body, "tree with absent gitlink");
}

#[test]
fn cat_nested_tree_directory_mode_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    std::fs::create_dir_all(wt.join("dir")).expect("mkdir");
    commit_file(wt, "dir/file", "nested\n", "nested dir");
    let spec = "HEAD^{tree}";
    let git_body = git_cat_file_p(wt, spec).expect("git -p");
    assert!(
        git_body.windows(6).any(|w| w == b"040000"),
        "git should print six-digit tree mode: {}",
        String::from_utf8_lossy(&git_body)
    );
    let repo = open_grit_repo(wt);
    let resolved = repo.resolve(spec).expect("resolve");
    let obj = repo
        .read_object_raw_for_display(&resolved.oid)
        .expect("read");
    let grit_body = format_tree_cat_file_p(&obj.data).expect("format tree");
    assert_eq!(grit_body, git_body, "nested tree cat-file -p");
}

#[test]
fn cat_tree_entry_type_follows_mode_not_stored_object_kind() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    std::fs::write(wt.join("blob.txt"), b"blob body\n").expect("write blob");
    std::fs::create_dir(wt.join("subdir")).expect("mkdir");
    std::fs::write(wt.join("subdir/nested.txt"), b"nested\n").expect("write nested");
    git(wt, &["add", "blob.txt", "subdir"]);
    git(wt, &["commit", "-m", "objects for mktree"]);

    let blob_oid = git_rev_parse_verify(wt, "HEAD:blob.txt").expect("blob oid");
    let subdir_tree_oid = git_rev_parse_verify(wt, "HEAD:subdir").expect("tree oid");

    let tree_oid = {
        let mut child = Command::new("git")
            .current_dir(wt)
            .args(["mktree"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("mktree");
        write!(
            child.stdin.as_mut().expect("stdin"),
            "100644 blob {}\twrong-kind-blob\n040000 tree {}\twrong-kind-tree\n",
            blob_oid,
            subdir_tree_oid,
        )
        .unwrap();
        let out = child.wait_with_output().expect("mktree wait");
        assert!(
            out.status.success(),
            "mktree: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };

    let spec = tree_oid.as_str();
    let git_body = git_cat_file_p(wt, spec).expect("git -p");
    let repo = open_grit_repo(wt);
    let oid = grit_lib::objects::ObjectId::from_hex(&tree_oid).expect("hex");
    let obj = repo.read_object_raw_for_display(&oid).expect("read tree");
    let grit_body = format_tree_cat_file_p(&obj.data).expect("format tree");
    assert_eq!(
        grit_body, git_body,
        "type words must follow mode bits like git cat-file -p"
    );
    assert!(
        git_body
            .split(|&b| b == b'\n')
            .any(|line| line.starts_with(format!("100644 blob {blob_oid}").as_bytes())),
        "git labels blob mode as blob even when oid is not a blob object"
    );
    assert!(
        git_body
            .split(|&b| b == b'\n')
            .any(|line| { line.starts_with(format!("040000 tree {subdir_tree_oid}").as_bytes()) }),
        "git labels tree mode as tree even when oid is not a tree object"
    );
}

#[test]
fn resolve_and_cat_follow_replace_refs() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "x\n", "init");
    let head = git_rev_parse_verify(wt, "HEAD").expect("HEAD");
    let blob_body = b"replacement";
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("hash-object");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(blob_body)
        .expect("write blob");
    let out = child.wait_with_output().expect("hash-object wait");
    assert!(out.status.success());
    let blob_hex = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let replace_dir = wt.join(".git/refs/replace");
    std::fs::create_dir_all(&replace_dir).expect("mkdir replace");
    std::fs::write(replace_dir.join(head.to_hex()), format!("{blob_hex}\n")).expect("replace ref");

    let repo = open_grit_repo(wt);
    let resolved = repo.resolve("HEAD").expect("resolve HEAD");
    assert_eq!(resolved.oid, head);
    assert_eq!(resolved.kind, ObjectKind::Blob);
    assert_eq!(resolved.size, blob_body.len() as u64);
    assert_eq!(
        git_cat_file_type(wt, "HEAD").as_deref(),
        Some("blob"),
        "git -t HEAD"
    );
    let git_body = git_cat_file_p(wt, "HEAD").expect("git -p");
    let obj = repo
        .read_object_raw_for_display(&resolved.oid)
        .expect("read replaced");
    assert_eq!(obj.data, git_body);
    assert_eq!(obj.data, blob_body);
}

#[test]
fn stream_for_display_follows_replace_refs_on_large_blob() {
    use std::io::Read;

    const SIZE: usize = 1024 * 1024 + 100;
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let original: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    let replacement: Vec<u8> = (0..SIZE + 73).map(|i| i.wrapping_mul(7) as u8).collect();
    let setup = open_grit_repo(wt);
    let oid_orig = setup
        .odb()
        .write(ObjectKind::Blob, &original)
        .expect("write original");
    let oid_repl = setup
        .odb()
        .write(ObjectKind::Blob, &replacement)
        .expect("write replacement");
    std::fs::create_dir_all(wt.join(".git/refs/replace")).expect("mkdir replace");
    std::fs::write(
        wt.join(".git/refs/replace").join(oid_orig.to_hex()),
        format!("{}\n", oid_repl.to_hex()),
    )
    .expect("replace ref");
    let repo = open_grit_repo(wt);

    let info = repo.read_object_info_for_display(&oid_orig).expect("info");
    assert_eq!(info.kind, ObjectKind::Blob);
    assert_eq!(info.size, replacement.len() as u64);
    let mut streamed = Vec::new();
    repo.with_object_stream_for_display(&oid_orig, |mut stream| {
        stream
            .reader
            .read_to_end(&mut streamed)
            .map_err(grit_lib::error::Error::Io)?;
        Ok(())
    })
    .expect("stream")
    .expect("object");
    assert_eq!(streamed, replacement);

    let mut env = Environment::empty();
    env.git_no_replace_objects = true;
    let repo = Repository::discover(Some(wt), &RepositoryOptions::with_environment(env))
        .expect("discover without replacements");
    assert_eq!(
        repo.read_object_info_for_display(&oid_orig)
            .expect("info")
            .size,
        original.len() as u64
    );
    streamed.clear();
    repo.with_object_stream_for_display(&oid_orig, |mut stream| {
        stream.reader.read_to_end(&mut streamed)?;
        Ok(())
    })
    .expect("stream without replacements")
    .expect("object");
    assert_eq!(streamed, original);
}

#[test]
fn display_stream_reports_missing_objects_and_callback_errors() {
    use grit_lib::error::Error;
    use grit_lib::objects::ObjectId;

    let hist = HistoryRepo::init();
    let repo = open_grit_repo(hist.worktree());
    let missing = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("oid");
    assert!(matches!(
        repo.read_object_info_for_display(&missing),
        Err(Error::ObjectNotFound(_))
    ));
    let result = repo
        .with_object_stream_for_display(&missing, |_| -> grit_lib::error::Result<()> {
            panic!("missing object must not call the callback")
        })
        .expect("missing object is not a stream error");
    assert_eq!(result, None);

    let oid = repo
        .odb()
        .write(ObjectKind::Blob, b"contents")
        .expect("blob");
    let error = repo
        .with_object_stream_for_display(&oid, |_| -> grit_lib::error::Result<()> {
            Err(Error::Io(std::io::Error::other("destination failed")))
        })
        .expect_err("callback failure");
    assert!(matches!(error, Error::Io(e) if e.to_string() == "destination failed"));
}

#[test]
fn display_stream_follows_commit_to_blob_replace_ref() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "x\n", "init");
    let head = git_rev_parse_verify(wt, "HEAD").expect("HEAD");
    let mut blob_body: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    blob_body[0] = 0;
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("hash-object");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(&blob_body)
        .expect("write blob");
    let out = child.wait_with_output().expect("hash-object wait");
    assert!(out.status.success());
    let blob_hex = String::from_utf8(out.stdout).unwrap().trim().to_string();
    std::fs::create_dir_all(wt.join(".git/refs/replace")).expect("mkdir replace");
    std::fs::write(
        wt.join(".git/refs/replace").join(head.to_hex()),
        format!("{blob_hex}\n"),
    )
    .expect("replace ref");

    let repo = open_grit_repo(wt);
    let mut streamed = Vec::new();
    let n = repo
        .with_object_stream_for_display(&head, |mut stream| {
            std::io::copy(&mut stream.reader, &mut streamed).map_err(grit_lib::error::Error::Io)
        })
        .expect("stream")
        .expect("present");
    assert_eq!(n, blob_body.len() as u64);
    assert_eq!(streamed, blob_body);
    assert_eq!(git_cat_file_p(wt, "HEAD").expect("git"), streamed);
}

#[test]
fn display_stream_propagates_writer_failures() {
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("writer failed"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "payload", "initial");
    let repo = open_grit_repo(wt);
    let missing = grit_lib::objects::ObjectId::from_hex("0000000000000000000000000000000000000000")
        .expect("oid");
    assert_eq!(
        repo.with_object_stream_for_display(&missing, |_| -> grit_lib::error::Result<u64> {
            panic!("missing object must not call writer")
        })
        .expect("missing object"),
        None
    );
    let oid = repo.resolve("HEAD:f.txt").expect("resolve").oid;
    assert!(matches!(
        repo.with_object_stream_for_display(&oid, |mut stream| {
            std::io::copy(&mut stream.reader, &mut FailingWriter)
                .map_err(grit_lib::error::Error::Io)
        }),
        Err(grit_lib::error::Error::Io(_))
    ));
}

#[test]
fn resolve_and_cat_respect_git_no_replace_objects() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "x\n", "init");
    let head = git_rev_parse_verify(wt, "HEAD").expect("HEAD");
    let blob_body = b"replacement";
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("hash-object");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(blob_body)
        .expect("write blob");
    let out = child.wait_with_output().expect("hash-object wait");
    assert!(out.status.success());
    let blob_hex = String::from_utf8(out.stdout).unwrap().trim().to_string();
    std::fs::create_dir_all(wt.join(".git/refs/replace")).expect("mkdir replace");
    std::fs::write(
        wt.join(".git/refs/replace").join(head.to_hex()),
        format!("{blob_hex}\n"),
    )
    .expect("write replace ref");

    let mut env = Environment::empty();
    env.git_no_replace_objects = true;
    let repo = Repository::discover(Some(wt), &RepositoryOptions::with_environment(env))
        .expect("discover");
    let resolved = repo.resolve("HEAD").expect("resolve HEAD");
    assert_eq!(resolved.kind, ObjectKind::Commit);
    assert_eq!(
        git_cat_file_type(wt, "HEAD").as_deref(),
        Some("blob"),
        "git still follows replace for HEAD spec"
    );
    let git_no_replace = Command::new("git")
        .current_dir(wt)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(["cat-file", "-t", "HEAD"])
        .output()
        .expect("git -t");
    assert!(git_no_replace.status.success());
    assert_eq!(
        String::from_utf8_lossy(&git_no_replace.stdout).trim(),
        "commit"
    );
    assert_eq!(kind_str(resolved.kind), "commit");
}

#[test]
fn ambiguous_short_oid_is_typed_error() {
    let (_dir, bare_repo, prefix) = bare_repo_with_ambiguous_prefix(8_000);
    let err = bare_repo.resolve(&prefix).expect_err("ambiguous");
    match err {
        ResolveError::AmbiguousShortObject { spec, candidates } => {
            assert_eq!(spec, prefix);
            assert!(candidates.len() > 1, "expected multiple candidates");
        }
        other => panic!("expected AmbiguousShortObject, got {other:?}"),
    }
}

fn kind_str(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}
