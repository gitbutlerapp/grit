//! Fetch over smart HTTP against the system `git http-backend`, protocol v0 and v2.
//!
//! Git's stateless upload-pack keeps nothing between POSTs and stops reading a
//! v0 request at the first flush after its haves, so every POST must restate the
//! want block plus the haves already acknowledged as common. These tests run the
//! library client against Git's own server and check the result with `git fsck`.

#![cfg(feature = "http-ureq")]

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use grit_lib::error::Result as GritResult;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchDepth, FetchOptions, FetchOutcome, TagMode};
use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
use grit_lib::remote::transport::http::{http_fetch, HttpClient};
use grit_test_support::git;
use grit_test_support::git_http_backend::GitHttpBackend;

fn git_out(dir: &Path, args: &[&str]) -> String {
    git(dir, args).trim().to_owned()
}

/// Append `count` commits to `branch` of `repo` (one file each), starting from
/// `from` and dated from `first_date` on, so they sort newest in date-ordered walks.
fn fast_import_commits(repo: &Path, branch: &str, from: &str, count: usize, first_date: u64) {
    let mut stream = String::new();
    for i in 0..count {
        let body = format!("{branch} {i}\n");
        stream.push_str(&format!(
            "commit refs/heads/{branch}\ncommitter T <t@example.com> {} +0000\ndata 2\nc\n",
            first_date + i as u64
        ));
        if i == 0 {
            stream.push_str(&format!("from {from}\n"));
        }
        stream.push_str(&format!(
            "M 100644 inline {branch}-{i}.txt\ndata {}\n{body}\n",
            body.len()
        ));
    }
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["fast-import", "--quiet"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stream.as_bytes())
        .expect("write fast-import stream");
    assert!(child.wait().expect("fast-import").success());
}

/// A bare `repo.git` under `root` with `commits` linear commits on `main`.
fn build_bare(root: &Path, commits: usize) -> PathBuf {
    let bare = root.join("repo.git");
    git(
        root,
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let mut stream = String::new();
    for i in 0..commits {
        stream.push_str(&format!(
            "commit refs/heads/main\ncommitter T <t@example.com> {} +0000\ndata 2\nc\nM 100644 inline f.txt\ndata {}\n{i}\n\n",
            1_000_000 + i as u64,
            format!("{i}\n").len()
        ));
    }
    let mut child = Command::new("git")
        .current_dir(&bare)
        .args(["fast-import", "--quiet"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stream.as_bytes())
        .expect("write fast-import stream");
    assert!(child.wait().expect("fast-import").success());
    bare
}

fn clone(root: &Path, url: &str, name: &str) -> PathBuf {
    let dest = root.join(name);
    git(root, &["clone", "-q", url, dest.to_str().unwrap()]);
    dest
}

fn fsck(work: &Path) {
    let out = Command::new("git")
        .current_dir(work)
        .args(["fsck", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("fsck");
    assert!(
        out.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn shallow_set(work: &Path) -> HashSet<String> {
    std::fs::read_to_string(work.join(".git/shallow"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Records every upload-pack POST body sent through the real client.
struct Recording {
    inner: UreqHttpClient,
    bodies: Mutex<Vec<Vec<u8>>>,
}

impl HttpClient for Recording {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> GritResult<Vec<u8>> {
        self.inner.get(url, git_protocol)
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> GritResult<Vec<u8>> {
        self.bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(body.to_vec());
        self.inner
            .post(url, content_type, accept, body, git_protocol)
    }

    fn git_protocol_header(&self) -> Option<&str> {
        self.inner.git_protocol_header()
    }
}

/// The pkt-lines of `body`, with `None` for a flush (or a v2 delimiter).
fn pkt_lines(body: &[u8]) -> Vec<Option<String>> {
    let mut out = Vec::new();
    let mut rest = body;
    while rest.len() >= 4 {
        let len = usize::from_str_radix(std::str::from_utf8(&rest[..4]).unwrap(), 16).unwrap();
        if len < 4 {
            out.push(None);
            rest = &rest[4..];
            continue;
        }
        out.push(Some(
            String::from_utf8_lossy(&rest[4..len]).trim_end().to_owned(),
        ));
        rest = &rest[len..];
    }
    out
}

fn grit_fetch(
    work: &Path,
    url: &str,
    refspec: &str,
    fetch_depth: Option<FetchDepth>,
    protocol: u8,
) -> (FetchOutcome, Vec<Vec<u8>>) {
    let inner = if protocol == 2 {
        UreqHttpClient::new().with_git_protocol("version=2")
    } else {
        UreqHttpClient::new()
    };
    let recording = Arc::new(Recording {
        inner,
        bodies: Mutex::new(Vec::new()),
    });
    let client: Arc<dyn HttpClient> = recording.clone();
    let mut opts = {
        let mut opts = FetchOptions::new();
        opts.refspecs = vec![refspec.to_owned()];
        opts.tags = TagMode::Following;
        opts.fetch_depth = fetch_depth;
        opts
    };
    let outcome = http_fetch(client, &work.join(".git"), url, &mut opts, &mut NoProgress)
        .unwrap_or_else(|e| panic!("protocol v{protocol} fetch: {e}"));
    let bodies = recording.bodies.lock().unwrap().clone();
    (outcome, bodies)
}

/// Each v0 POST is one complete request: the want block, then haves ended by a
/// single flush or by `done`. A replayed earlier round would put a second flush
/// before the end, which Git's server never reads past.
fn assert_v0_bodies_are_single_requests(bodies: &[Vec<u8>]) {
    for body in bodies {
        let lines = pkt_lines(body);
        let want_end = lines
            .iter()
            .position(Option::is_none)
            .expect("want block flush");
        let after = &lines[want_end + 1..];
        let flushes = after.iter().filter(|l| l.is_none()).count();
        assert!(
            flushes <= 1 && after.iter().rev().skip(1).all(Option::is_some),
            "request restates earlier rounds: {lines:?}"
        );
        assert!(
            !lines.iter().flatten().any(|l| l == "deepen-relative"),
            "v0 carries deepen-relative only as a capability"
        );
    }
}

/// Many local-only branch tips make the client offer several rounds of haves
/// before it reaches history the server knows. Every round must reach Git's server as a
/// complete request, or the final `done` is never read and no pack arrives.
#[test]
fn multi_round_negotiation_over_git_http_backend_lands_the_new_tip() {
    for protocol in [0u8, 2] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("srv");
        std::fs::create_dir_all(&root).unwrap();
        let bare = build_bare(&root, 40);
        let server = GitHttpBackend::start(&root).expect("git http-backend");
        let url = server.url("repo.git");

        let client = clone(tmp.path(), &url, "client");
        for i in 0..60 {
            fast_import_commits(
                &client,
                &format!("local-{i}"),
                "refs/heads/main",
                1,
                5_000_000 + i,
            );
        }
        fast_import_commits(&bare, "main", "refs/heads/main^0", 1, 4_000_000);
        let tip = git_out(&bare, &["rev-parse", "main"]);

        let (_, bodies) = grit_fetch(
            &client,
            &url,
            "+refs/heads/*:refs/remotes/origin/*",
            None,
            protocol,
        );
        assert_eq!(git_out(&client, &["rev-parse", "origin/main"]), tip);
        fsck(&client);
        let have_rounds = bodies
            .iter()
            .filter(|b| {
                pkt_lines(b)
                    .iter()
                    .flatten()
                    .any(|l| l.starts_with("have "))
            })
            .count();
        assert!(have_rounds >= 2, "v{protocol}: {have_rounds} have round(s)");
        if protocol == 0 {
            assert_v0_bodies_are_single_requests(&bodies);
        }
    }
}

/// The benchmark shape: a full clone made shallow by `fetch --depth 1`, then
/// deepened. Its other refs hold everything below the old boundary, so with
/// haves only the commits between are sent, and the new boundary matches git's.
#[test]
fn deepen_of_refetched_shallow_tip_over_git_http_backend_matches_git() {
    for protocol in [0u8, 2] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("srv");
        std::fs::create_dir_all(&root).unwrap();
        let bare = build_bare(&root, 12);
        git(&bare, &["branch", "old", "main~2"]);
        let server = GitHttpBackend::start(&root).expect("git http-backend");
        let url = server.url("repo.git");

        let mut clients = Vec::new();
        for name in ["git-client", "grit-client"] {
            let dest = clone(tmp.path(), &url, name);
            git(&dest, &["fetch", "-q", "--depth", "1", "origin", "main"]);
            assert_eq!(shallow_set(&dest).len(), 1);
            clients.push(dest);
        }
        let (git_client, grit_client) = (&clients[0], &clients[1]);
        let version = format!("protocol.version={protocol}");
        git(
            git_client,
            &[
                "-c", &version, "fetch", "-q", "--deepen", "3", "origin", "main",
            ],
        );
        let (outcome, bodies) = grit_fetch(
            grit_client,
            &url,
            "+refs/heads/main:refs/remotes/origin/main",
            FetchDepth::deepen(3),
            protocol,
        );
        assert_eq!(
            shallow_set(git_client),
            shallow_set(grit_client),
            "v{protocol}"
        );
        fsck(grit_client);
        // main~1's commit, tree and blob: main~2 and below are held through `old`.
        assert_eq!(outcome.transfer.objects, 3, "v{protocol}");
        if protocol == 0 {
            assert_v0_bodies_are_single_requests(&bodies);
        }
    }
}

/// A deepen of a plain depth-1 clone: the deepened tip is offered as a have, so
/// the server sends only the new commits and not the tip's tree again.
#[test]
fn deepen_of_depth_one_clone_over_git_http_backend_sends_only_new_history() {
    for protocol in [0u8, 2] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("srv");
        std::fs::create_dir_all(&root).unwrap();
        build_bare(&root, 12);
        let server = GitHttpBackend::start(&root).expect("git http-backend");
        let url = server.url("repo.git");

        let mut clients = Vec::new();
        for name in ["git-client", "grit-client"] {
            let dest = tmp.path().join(name);
            git(
                tmp.path(),
                &["clone", "-q", "--depth", "1", &url, dest.to_str().unwrap()],
            );
            clients.push(dest);
        }
        let (git_client, grit_client) = (&clients[0], &clients[1]);
        let version = format!("protocol.version={protocol}");
        git(
            git_client,
            &["-c", &version, "fetch", "-q", "--deepen", "3", "origin"],
        );
        let (outcome, bodies) = grit_fetch(
            grit_client,
            &url,
            "+refs/heads/*:refs/remotes/origin/*",
            FetchDepth::deepen(3),
            protocol,
        );
        assert_eq!(
            shallow_set(git_client),
            shallow_set(grit_client),
            "v{protocol}"
        );
        assert_eq!(
            git_out(grit_client, &["rev-list", "--count", "HEAD"]),
            "4",
            "v{protocol}"
        );
        fsck(grit_client);
        // Three new commits, each with its own tree and blob.
        assert_eq!(outcome.transfer.objects, 9, "v{protocol}");
        if protocol == 0 {
            assert_v0_bodies_are_single_requests(&bodies);
        }
    }
}
