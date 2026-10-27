//! Shallow fetches with `include-tag` from a repository with annotated tags
//! (gitbutlerapp/grit#1734): in-process [`upload_pack`] must answer the same
//! protocol v2 request as system `git upload-pack --stateless-rpc` with the
//! same `shallow` lines and the same pack objects.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::remote::serve::{upload_pack, ProtocolVersion, ServeOptions};
use grit_lib::repo::Repository;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

/// 30 linear commits on `main` with annotated tags `v10`, `v20`, `v30`; commit
/// `i` is dated `1_700_000_000 + i * 86_400`.
fn tagged_bare_repo(root: &Path) -> std::path::PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    for i in 1..=30 {
        std::fs::write(work.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(&work, &["add", "."]);
        let date = format!("{} +0000", 1_700_000_000 + i * 86_400);
        let out = Command::new("git")
            .current_dir(&work)
            .args(["commit", "-q", "-m", &format!("c{i}")])
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success());
        if i % 10 == 0 {
            git(&work, &["tag", "-a", "-m", "t", &format!("v{i}")]);
        }
    }
    let bare = root.join("src.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn pkt_line(buf: &mut Vec<u8>, line: &str) {
    let payload = format!("{line}\n");
    buf.extend(format!("{:04x}", payload.len() + 4).as_bytes());
    buf.extend(payload.as_bytes());
}

fn v2_fetch_body(wants: &[String], args: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    pkt_line(&mut body, "command=fetch");
    pkt_line(&mut body, "object-format=sha1");
    body.extend(b"0001");
    for arg in ["ofs-delta", "no-progress", "include-tag"] {
        pkt_line(&mut body, arg);
    }
    for want in wants {
        pkt_line(&mut body, &format!("want {want}"));
    }
    for arg in args {
        pkt_line(&mut body, arg);
    }
    pkt_line(&mut body, "done");
    body.extend(b"0000");
    body
}

/// Distinct `shallow` lines (sorted) and the raw pack from a v2 fetch response.
///
/// Git repeats a border once per want that reaches it (`main` and `v30` name
/// the same commit); the client stores a set, so only the set is compared.
fn parse_v2_fetch_response(resp: &[u8]) -> (Vec<String>, Vec<u8>) {
    let mut shallow = Vec::new();
    let mut pack = Vec::new();
    let mut in_pack = false;
    let mut i = 0;
    while i + 4 <= resp.len() {
        let len = usize::from_str_radix(std::str::from_utf8(&resp[i..i + 4]).unwrap(), 16).unwrap();
        if len < 4 {
            i += 4;
            continue;
        }
        let payload = &resp[i + 4..i + len];
        i += len;
        if in_pack {
            if payload[0] == 1 {
                pack.extend_from_slice(&payload[1..]);
            }
            continue;
        }
        let line = String::from_utf8_lossy(payload).trim_end().to_owned();
        if line == "packfile" {
            in_pack = true;
        } else if let Some(oid) = line.strip_prefix("shallow ") {
            shallow.push(oid.to_owned());
        }
    }
    shallow.sort();
    shallow.dedup();
    (shallow, pack)
}

fn grit_fetch(bare: &Path, body: &[u8]) -> Vec<u8> {
    let repo = Repository::open(bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(body.to_vec()),
        &mut out,
        &mut ServeOptions {
            protocol: ProtocolVersion::V2,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-test".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect("grit upload-pack");
    out
}

fn git_fetch(bare: &Path, body: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(["upload-pack", "--stateless-rpc"])
        .arg(bare)
        .env("GIT_PROTOCOL", "version=2")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git upload-pack");
    child.stdin.take().unwrap().write_all(body).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git upload-pack failed");
    out.stdout
}

/// Sorted `<oid> <type>` lines for every object in `pack`, indexed by system git.
fn pack_objects_listing(root: &Path, name: &str, pack: &[u8]) -> Vec<String> {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "--bare"]);
    let mut child = Command::new("git")
        .current_dir(&dir)
        .args(["index-pack", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(pack).unwrap();
    assert!(child.wait().unwrap().success(), "index-pack {name}");
    let listing = git(
        &dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    );
    let mut lines: Vec<String> = listing.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

/// Run the same request against grit and git; assert identical shallow lines
/// and pack contents, and return grit's (shallow, objects).
fn assert_matches_git(
    root: &Path,
    bare: &Path,
    label: &str,
    body: &[u8],
) -> (Vec<String>, Vec<String>) {
    let (grit_shallow, grit_pack) = parse_v2_fetch_response(&grit_fetch(bare, body));
    let (git_shallow, git_pack) = parse_v2_fetch_response(&git_fetch(bare, body));
    assert_eq!(grit_shallow, git_shallow, "{label}: shallow lines differ");
    let grit_objects = pack_objects_listing(root, &format!("{label}-grit"), &grit_pack);
    let git_objects = pack_objects_listing(root, &format!("{label}-git"), &git_pack);
    assert_eq!(grit_objects, git_objects, "{label}: pack objects differ");
    (grit_shallow, grit_objects)
}

fn rev_parse(bare: &Path, spec: &str) -> String {
    git(bare, &["rev-parse", spec]).trim().to_owned()
}

fn count_kind(objects: &[String], kind: &str) -> usize {
    objects.iter().filter(|l| l.ends_with(kind)).count()
}

#[test]
fn depth_one_on_branch_sends_one_commit_and_its_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tagged_bare_repo(tmp.path());
    let body = v2_fetch_body(&[rev_parse(&bare, "main")], &["deepen 1"]);
    let (shallow, objects) = assert_matches_git(tmp.path(), &bare, "branch", &body);
    assert_eq!(shallow, vec![rev_parse(&bare, "main")]);
    assert_eq!(count_kind(&objects, " commit"), 1);
    assert_eq!(count_kind(&objects, " tag"), 1, "v30 tags the tip");
    assert_eq!(objects.len(), 33);
}

#[test]
fn depth_one_with_tag_wants_advertises_only_commit_borders() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tagged_bare_repo(tmp.path());
    let wants: Vec<String> = ["main", "v10", "v20", "v30"]
        .iter()
        .map(|r| rev_parse(&bare, &format!("refs/{}", ref_path(r))))
        .collect();
    let body = v2_fetch_body(&wants, &["deepen 1"]);
    let (shallow, objects) = assert_matches_git(tmp.path(), &bare, "tags", &body);
    let mut expected: Vec<String> = ["main", "v10^{commit}", "v20^{commit}"]
        .iter()
        .map(|r| rev_parse(&bare, r))
        .collect();
    expected.sort();
    assert_eq!(shallow, expected, "shallow lines must name commits");
    assert_eq!(count_kind(&objects, " commit"), 3);
    assert_eq!(count_kind(&objects, " tag"), 3);
    assert_eq!(objects.len(), 39);
}

#[test]
fn deepen_since_does_not_pull_in_tagged_history() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tagged_bare_repo(tmp.path());
    let since = format!("deepen-since {}", 1_700_000_000 + 25 * 86_400);
    let body = v2_fetch_body(&[rev_parse(&bare, "main")], &[&since]);
    let (_, objects) = assert_matches_git(tmp.path(), &bare, "since", &body);
    assert_eq!(count_kind(&objects, " commit"), 6);
    assert_eq!(count_kind(&objects, " tag"), 1);
}

#[test]
fn depth_reaching_a_tagged_commit_includes_that_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tagged_bare_repo(tmp.path());
    let body = v2_fetch_body(&[rev_parse(&bare, "main")], &["deepen 11"]);
    let (_, objects) = assert_matches_git(tmp.path(), &bare, "deep11", &body);
    assert_eq!(count_kind(&objects, " commit"), 11);
    assert_eq!(count_kind(&objects, " tag"), 2, "v30 and v20");
}

fn ref_path(name: &str) -> String {
    if name == "main" {
        "heads/main".to_owned()
    } else {
        format!("tags/{name}")
    }
}
