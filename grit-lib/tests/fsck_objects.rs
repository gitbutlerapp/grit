//! Object-level fsck scenarios from upstream `t1450-fsck.sh`, cross-checked against
//! system `git fsck` msg-ids and [`grit_lib::fsck_standalone::fsck_object`].

use std::collections::HashSet;
use std::process::Command;

use grit_lib::fsck_standalone::{fsck_object, FsckError, FsckObjectOptions};
use grit_lib::gitmodules::{fsck_dot_special_object, fsck_dot_special_tree_pass, DotFsckTracker};
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_test_support::{
    git_fsck, git_hash_object_literally, git_supports_sha256, write_loose_object,
    HashAlgo as FixtureAlgo, RepoFixture,
};

const SHA1_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const SHA256_TREE: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

fn tree_oid_hex(algo: FixtureAlgo) -> &'static str {
    match algo {
        FixtureAlgo::Sha1 => SHA1_TREE,
        FixtureAlgo::Sha256 => SHA256_TREE,
    }
}

fn fsck_options(algo: FixtureAlgo) -> FsckObjectOptions {
    FsckObjectOptions::new(match algo {
        FixtureAlgo::Sha1 => HashAlgo::Sha1,
        FixtureAlgo::Sha256 => HashAlgo::Sha256,
    })
}

fn wrong_width_tree_oid(algo: FixtureAlgo) -> &'static str {
    match algo {
        FixtureAlgo::Sha1 => SHA256_TREE,
        FixtureAlgo::Sha256 => SHA1_TREE,
    }
}

fn valid_commit(algo: FixtureAlgo) -> Vec<u8> {
    format!(
        "tree {}\nauthor A U Thor <author@example.com> 1 +0000\ncommitter C O Mitter <committer@example.com> 1 +0000\n\n",
        tree_oid_hex(algo)
    )
    .into_bytes()
}

fn valid_tag(algo: FixtureAlgo) -> Vec<u8> {
    let object = tree_oid_hex(algo);
    format!(
        "object {object}\ntype commit\ntag valid-tag\ntagger T A Gger <tagger@example.com> 1234567890 +0000\n\nmessage\n"
    )
    .into_bytes()
}

fn grit_kind(kind: &str) -> ObjectKind {
    match kind {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "commit" => ObjectKind::Commit,
        "tag" => ObjectKind::Tag,
        _ => panic!("unknown kind {kind}"),
    }
}

#[derive(Clone, Copy)]
enum GitVerdictSource {
    /// `git hash-object -t … --stdin` (same buffer fsck as `hash-object -w`).
    HashObject,
    /// `git fsck` on a repo containing the loose object (`--literally` written).
    RepoFsck,
}

struct ObjectCase {
    name: &'static str,
    kind: &'static str,
    body: Vec<u8>,
    /// Expected camelCase msg-id when rejected; `None` when the object should pass standalone fsck.
    expect_id: Option<&'static str>,
    git_strict: bool,
    git_tags: bool,
    /// Extra `git -c key=value` pairs applied before `git fsck`.
    git_config: &'static [(&'static str, &'static str)],
    git_verdict: Option<GitVerdictSource>,
}

fn git_hash_object_fsck(
    repo: &RepoFixture,
    kind: &str,
    body: &[u8],
    git_config: &[(&str, &str)],
) -> (bool, Vec<String>) {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo.path());
    for (k, v) in git_config {
        cmd.args(["-c", &format!("{k}={v}")]);
    }
    cmd.args(["hash-object", "-t", kind, "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn hash-object");
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(body);
    }
    let out = child.wait_with_output().expect("hash-object");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut ids = Vec::new();
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix("error: object fails fsck: ") else {
            continue;
        };
        let Some(id) = rest.split(':').next() else {
            continue;
        };
        let id = id.trim();
        if is_msg_id(id) {
            ids.push(id.to_string());
        }
    }
    (out.status.success(), ids)
}

fn install_loose_and_git_fsck(
    repo: &RepoFixture,
    kind: &str,
    body: &[u8],
    strict: bool,
    tags: bool,
    git_config: &[(&str, &str)],
) -> grit_test_support::FsckOutcome {
    let oid = git_hash_object_literally(repo.path(), repo.algo(), kind, body)
        .expect("git hash-object --literally");
    if matches!(kind, "commit" | "tag") {
        let _ = Command::new("git")
            .current_dir(repo.path())
            .args(["update-ref", "refs/heads/fsck-probe", &oid])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output();
    }
    let mut cmd = Command::new("git");
    cmd.current_dir(repo.path());
    for (k, v) in git_config {
        cmd.args(["-c", &format!("{k}={v}")]);
    }
    let mut args = vec!["fsck", "--no-dangling"];
    if strict {
        args.push("--strict");
    }
    if tags {
        args.push("--tags");
    }
    cmd.args(&args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().expect("git fsck");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    grit_test_support::FsckOutcome {
        ok: out.status.success(),
        msg_ids: parse_git_msg_ids(&combined),
    }
}

fn parse_git_msg_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let rest = line
            .strip_prefix("error: ")
            .or_else(|| line.strip_prefix("warning: "))
            .or_else(|| line.strip_prefix("error in tree "))
            .or_else(|| line.strip_prefix("error in commit "))
            .or_else(|| line.strip_prefix("error in blob "))
            .or_else(|| line.strip_prefix("error in tag "))
            .or_else(|| line.strip_prefix("warning in tree "))
            .or_else(|| line.strip_prefix("warning in blob "))
            .or_else(|| line.strip_prefix("warning in tag "));
        let Some(rest) = rest else {
            continue;
        };
        for segment in rest.split(':') {
            let id = segment.trim();
            if is_msg_id(id) && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

fn is_msg_id(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes().any(|b| b.is_ascii_uppercase())
        && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn dot_issue_id(issue: &grit_lib::gitmodules::DotFsckIssue) -> &'static str {
    match issue {
        grit_lib::gitmodules::DotFsckIssue::TreeSymlink { id, .. }
        | grit_lib::gitmodules::DotFsckIssue::NonBlobDotFile { id, .. }
        | grit_lib::gitmodules::DotFsckIssue::BlobGitmodules { id, .. }
        | grit_lib::gitmodules::DotFsckIssue::BlobGitattributes { id, .. } => id,
    }
}

fn assert_case(algo: FixtureAlgo, case: &ObjectCase) {
    let repo = RepoFixture::init(algo).expect("init repo");
    let kind = grit_kind(case.kind);

    let grit_result = fsck_object(kind, &case.body, fsck_options(algo));
    let grit_id = grit_result.as_ref().err().map(|e| e.id);
    let grit_ok = grit_result.is_ok();

    if let Some(expected) = case.expect_id {
        assert!(
            !grit_ok,
            "{} ({algo:?}): expected grit reject with {expected}, got Ok",
            case.name
        );
        assert_eq!(
            grit_id,
            Some(expected),
            "{} ({algo:?}): grit msg-id",
            case.name
        );
    } else {
        assert!(
            grit_ok,
            "{} ({algo:?}): grit rejected: {grit_id:?}",
            case.name
        );
    }

    let git_repo = install_loose_and_git_fsck(
        &repo,
        case.kind,
        &case.body,
        case.git_strict,
        case.git_tags,
        case.git_config,
    );

    let verdict_source = case.git_verdict.unwrap_or(GitVerdictSource::HashObject);
    let (git_hash_ok, git_hash_ids) =
        git_hash_object_fsck(&repo, case.kind, &case.body, case.git_config);

    match verdict_source {
        GitVerdictSource::HashObject => {
            assert_eq!(
                git_hash_ok, grit_ok,
                "{} ({algo:?}): hash-object fsck vs grit mismatch (git ids {git_hash_ids:?})",
                case.name
            );
        }
        GitVerdictSource::RepoFsck => {
            assert_eq!(
                git_repo.ok, grit_ok,
                "{} ({algo:?}): repo fsck vs grit mismatch (ids {:?})",
                case.name, git_repo.msg_ids
            );
        }
    }

    if let Some(expected) = case.expect_id {
        assert_eq!(
            grit_id,
            Some(expected),
            "{} ({algo:?}): grit msg-id",
            case.name
        );
        let git_ids = match verdict_source {
            GitVerdictSource::HashObject => &git_hash_ids,
            GitVerdictSource::RepoFsck => &git_repo.msg_ids,
        };
        assert!(
            git_ids.iter().any(|id| id == expected),
            "{} ({algo:?}): git missing msg-id {expected}: {git_ids:?}",
            case.name
        );
    }
}

fn run_algo(algo: FixtureAlgo, f: impl FnOnce(FixtureAlgo)) {
    if matches!(algo, FixtureAlgo::Sha256) && !git_supports_sha256() {
        eprintln!("SKIP: system git lacks sha256 object format");
        return;
    }
    f(algo);
}

fn object_cases(algo: FixtureAlgo) -> Vec<ObjectCase> {
    let tree = tree_oid_hex(algo);
    let junk_parent = if matches!(algo, FixtureAlgo::Sha1) {
        "not_valid_tree_sha1_line_format_xxxxx"
    } else {
        "not_valid_tree_sha256_line_format_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    };
    let mut cases = vec![
        ObjectCase {
            name: "blob always ok",
            kind: "blob",
            body: b"any bytes".to_vec(),
            expect_id: None,
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit missing tree",
            kind: "commit",
            body: b"\n\n".to_vec(),
            expect_id: Some("missingTree"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit missing author",
            kind: "commit",
            body: format!("tree {tree}\ncommitter C <c@e.com> 1 +0000\n\n").into_bytes(),
            expect_id: Some("missingAuthor"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit missing committer",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("missingCommitter"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit bad tree oid",
            kind: "commit",
            body: format!(
                "tree {junk_parent}\nauthor A <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badTreeSha1"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit tree oid wrong hash width",
            kind: "commit",
            body: format!(
                "tree {}\nauthor A <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n",
                wrong_width_tree_oid(algo)
            )
            .into_bytes(),
            expect_id: Some("badTreeSha1"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit bad parent oid",
            kind: "commit",
            body: format!(
                "tree {tree}\nparent {junk_parent}\nauthor A <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badParentSha1"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit multiple authors",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com> 1 +0000\nauthor B <b@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("multipleAuthors"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit missing email brackets",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A author@example.com 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("missingEmail"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit bad timezone",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com> 1 BADTZ\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badTimezone"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit date overflow",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com> 18446744073709551617 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badDateOverflow"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit zero-padded date",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com> 01 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("zeroPaddedDate"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit bad name",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A > <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badName"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit bad email",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badEmail"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit missing space before email",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A<a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("missingSpaceBeforeEmail"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit missing name before email",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("missingNameBeforeEmail"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit bad date token",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com> notnum +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badDate"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit missing space before date",
            kind: "commit",
            body: format!(
                "tree {tree}\nauthor A <a@e.com>1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("missingSpaceBeforeDate"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit well-formed",
            kind: "commit",
            body: valid_commit(algo),
            expect_id: None,
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit nul in header",
            kind: "commit",
            body: {
                let mut b = valid_commit(algo);
                if let Some(i) = b.windows(6).position(|w| w == b"author") {
                    b[i + 6] = 0;
                }
                b
            },
            expect_id: Some("nulInHeader"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "commit nul in body",
            kind: "commit",
            body: {
                let mut b = valid_commit(algo);
                b.push(0);
                b
            },
            expect_id: Some("nulInCommit"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree bad sort order",
            kind: "tree",
            body: {
                let oid = hex::decode(tree).expect("hex");
                let mut out = b"100644 b\0".to_vec();
                out.extend_from_slice(&oid);
                out.extend_from_slice(b"100644 a\0");
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("treeNotSorted"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree duplicate entries",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = Vec::new();
                out.extend_from_slice(b"100644 dup\0");
                out.extend_from_slice(&oid);
                out.extend_from_slice(b"100644 dup\0");
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("duplicateEntries"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree zero-padded mode",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"0100644 padded\0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("zeroPaddedFilemode"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree bad mode",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100000 badmode\0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("badFilemode"),
            git_strict: true,
            git_tags: false,
            git_config: &[("fsck.badFilemode", "error")],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree empty name",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100644 \0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("badTree"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree has dot",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100644 .\0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("hasDot"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree has dotdot",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100644 ..\0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("hasDotdot"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree has dotgit",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100644 .git\0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("hasDotgit"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree full path",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100644 foo/bar\0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("fullPathname"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree null sha1",
            kind: "tree",
            body: {
                let oid_len = match algo {
                    FixtureAlgo::Sha1 => 20,
                    FixtureAlgo::Sha256 => 32,
                };
                let mut out = b"100644 f\0".to_vec();
                out.extend_from_slice(&vec![0u8; oid_len]);
                out
            },
            expect_id: Some("nullSha1"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree large pathname",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100644 ".to_vec();
                out.extend_from_slice(&[b'x'; 5000]);
                out.push(0);
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("largePathname"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree ntfs backslash dotgit",
            kind: "tree",
            body: {
                let blob = tree;
                let oid = hex::decode(blob).expect("hex");
                let mut out = b"100644 foo\\.git\0".to_vec();
                out.extend_from_slice(&oid);
                out
            },
            expect_id: Some("hasDotgit"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree truncated oid",
            kind: "tree",
            body: b"100644 foo\0\x01\x02\x03".to_vec(),
            expect_id: Some("badTree"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tree missing nul",
            kind: "tree",
            body: b"100644 foo".to_vec(),
            expect_id: Some("badTree"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag missing object",
            kind: "tag",
            body: b"type commit\ntag t\ntagger T <t@e.com> 1 +0000\n\n".to_vec(),
            expect_id: Some("missingObject"),
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag bad object oid",
            kind: "tag",
            body: format!(
                "object {junk_parent}\ntype commit\ntag t\ntagger T <t@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badObjectSha1"),
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag bad type",
            kind: "tag",
            body: format!(
                "object {tree}\ntype bogus\ntag t\ntagger T <t@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badType"),
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag bad name",
            kind: "tag",
            body: format!(
                "object {tree}\ntype commit\ntag bad name\ntagger T <t@e.com> 1 +0000\n\n"
            )
            .into_bytes(),
            expect_id: Some("badTagName"),
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag missing tagger",
            kind: "tag",
            body: format!("object {tree}\ntype commit\ntag t\n\n").into_bytes(),
            expect_id: Some("missingTaggerEntry"),
            git_strict: true,
            git_tags: true,
            git_config: &[("fsck.missingTaggerEntry", "error")],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag bad tagger",
            kind: "tag",
            body: format!(
                "object {tree}\ntype commit\ntag ok\ntagger Bad Tagger\n\n"
            )
            .into_bytes(),
            expect_id: Some("missingEmail"),
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag nul in header",
            kind: "tag",
            body: {
                let mut b = valid_tag(algo);
                if let Some(i) = b.windows(6).position(|w| w == b"tagger") {
                    b[i + 6] = 0;
                }
                b
            },
            expect_id: Some("nulInHeader"),
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag extra header after tagger",
            kind: "tag",
            body: format!(
                "object {tree}\ntype commit\ntag ok\ntagger T <t@e.com> 1 +0000\njunk\n\n"
            )
            .into_bytes(),
            expect_id: Some("extraHeaderEntry"),
            git_strict: true,
            git_tags: true,
            git_config: &[("fsck.extraHeaderEntry", "error")],
            git_verdict: Some(GitVerdictSource::RepoFsck),
        },
        ObjectCase {
            name: "tag well-formed",
            kind: "tag",
            body: valid_tag(algo),
            expect_id: None,
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
        ObjectCase {
            name: "tag gpgsig-sha256",
            kind: "tag",
            body: format!(
                "object {tree}\ntype commit\ntag signed\ntagger T <t@e.com> 1 +0000\ngpgsig-sha256 sig\n\nbody\n"
            )
            .into_bytes(),
            expect_id: None,
            git_strict: true,
            git_tags: true,
            git_config: &[],
            git_verdict: None,
        },
    ];

    // HFS / NTFS `.git` spellings (t1450 fsck notices …).
    for (name, path) in [
        ("tree hfs dotgit unicode", ".gI\u{200c}T"),
        ("tree ntfs dotgit", ".git."),
        ("tree ntfs git tilde1", "git~1"),
    ] {
        let blob = tree;
        let oid = hex::decode(blob).expect("hex");
        let mut out = b"100644 ".to_vec();
        out.extend_from_slice(path.as_bytes());
        out.push(0);
        out.extend_from_slice(&oid);
        cases.push(ObjectCase {
            name,
            kind: "tree",
            body: out,
            expect_id: Some("hasDotgit"),
            git_strict: true,
            git_tags: false,
            git_config: &[],
            git_verdict: None,
        });
    }

    cases
}

#[test]
fn fsck_object_cases_match_git_sha1() {
    run_algo(FixtureAlgo::Sha1, |algo| {
        for case in object_cases(algo) {
            assert_case(algo, &case);
        }
    });
}

#[test]
fn fsck_object_cases_match_git_sha256() {
    run_algo(FixtureAlgo::Sha256, |algo| {
        for case in object_cases(algo) {
            assert_case(algo, &case);
        }
    });
}

#[test]
fn fsck_error_report_line_is_stable() {
    let err = FsckError::new("missingTree", "commit header must start with a tree line");
    assert_eq!(
        err.report_line(),
        "missingTree: commit header must start with a tree line"
    );
}

#[test]
fn gitmodules_blob_fsck_via_dot_special_object_sha1() {
    run_algo(FixtureAlgo::Sha1, |algo| {
        gitmodules_blob_fsck_via_dot_special_object_inner(algo);
    });
}

#[test]
fn gitmodules_blob_fsck_via_dot_special_object_sha256() {
    run_algo(FixtureAlgo::Sha256, |algo| {
        gitmodules_blob_fsck_via_dot_special_object_inner(algo);
    });
}

fn gitmodules_blob_fsck_via_dot_special_object_inner(algo: FixtureAlgo) {
    let repo = RepoFixture::init(algo).expect("init");
    let bad = b"[submodule \"x\"]\n\tpath = -oops\n\turl = https://example.com\n";
    let blob_oid = write_loose_object(&repo.objects_dir(), algo, "blob", bad).expect("write blob");
    let blob_oid: ObjectId = blob_oid.parse().expect("parse oid");

    let tree_body = {
        let mut out = b"100644 .gitmodules\0".to_vec();
        out.extend_from_slice(blob_oid.as_bytes());
        out
    };
    let tree_hex = write_loose_object(&repo.objects_dir(), algo, "tree", &tree_body).expect("tree");
    let tree_oid: ObjectId = tree_hex.parse().expect("parse tree oid");

    let mut gm = HashSet::new();
    let mut ga = HashSet::new();
    let tree_issues =
        fsck_dot_special_tree_pass(&tree_oid, &tree_body, &mut gm, &mut ga).expect("tree pass");
    assert!(tree_issues.is_empty());
    assert!(gm.contains(&blob_oid));

    let issues = fsck_dot_special_object(&blob_oid, ObjectKind::Blob, bad, &gm, &ga);
    assert!(
        issues.iter().any(|i| dot_issue_id(i) == "gitmodulesPath"),
        "expected gitmodulesPath, got {:?}",
        issues
    );

    let fsck = git_fsck(repo.path(), true);
    assert!(
        fsck.msg_ids.iter().any(|id| id == "gitmodulesPath"),
        "git fsck msg-ids: {:?}",
        fsck.msg_ids
    );
}

#[test]
fn dot_fsck_tracker_finish_pending_sha1() {
    run_algo(FixtureAlgo::Sha1, |algo| {
        dot_fsck_tracker_finish_pending_inner(algo);
    });
}

fn dot_fsck_tracker_finish_pending_inner(algo: FixtureAlgo) {
    use grit_lib::repo::Repository;

    let repo = RepoFixture::init(algo).expect("init");
    let bad = b"[submodule \"x\"]\n\tpath = -oops\n\turl = https://example.com\n";
    let blob_hex = write_loose_object(&repo.objects_dir(), algo, "blob", bad).expect("write blob");
    let blob_oid: ObjectId = blob_hex.parse().expect("oid");
    let tree_body = {
        let mut out = b"100644 .gitmodules\0".to_vec();
        out.extend_from_slice(blob_oid.as_bytes());
        out
    };
    let tree_hex = write_loose_object(&repo.objects_dir(), algo, "tree", &tree_body).expect("tree");
    let tree_oid: ObjectId = tree_hex.parse().expect("oid");

    let grit_repo =
        Repository::open(&repo.path().join(".git"), Some(repo.path())).expect("open grit repo");
    let odb = &grit_repo.odb;
    let mut tracker = DotFsckTracker::default();
    tracker.on_tree(&tree_oid, &tree_body).expect("on_tree");
    let pending = tracker.finish_pending(&odb).expect("finish");
    assert!(
        pending.iter().any(|i| dot_issue_id(i) == "gitmodulesPath"),
        "{pending:?}"
    );
}
