//! `RevWalk::resolve` with `ResolveOptions` against system `git rev-parse` on the same
//! repository: peeling, ranges, and abbreviated hex that matches several objects.

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::hash::hash_object;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::revwalk::{IndexDwim, MergeBaseMode, Peel, ResolveOptions, ResolvePurpose};
use grit_test_support::{git, git_cmd};

fn git_oid(dir: &Path, args: &[&str]) -> Option<ObjectId> {
    let out = git_cmd(args).in_dir(dir).exec();
    if out.status != Some(0) {
        return None;
    }
    out.stdout.trim().parse().ok()
}

fn commit_at(dir: &Path, message: &str, time: u32) {
    let date = format!("{} +0000", 1_700_000_000 + time);
    git_cmd(&["commit", "-q", "-m", message])
        .in_dir(dir)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .suc();
}

/// Data for an object of `kind` whose id shares the first four hex digits with `target`.
fn colliding(kind: ObjectKind, target: ObjectId, make: impl Fn(u64) -> Vec<u8>) -> Vec<u8> {
    let prefix = &target.to_hex()[..4];
    (0u64..)
        .map(make)
        .find(|data| {
            let oid = hash_object(HashAlgo::Sha1, kind, data).expect("hash");
            oid != target && oid.to_hex().starts_with(prefix)
        })
        .expect("colliding object")
}

fn write_with_git(dir: &Path, kind: &str, data: &[u8]) -> ObjectId {
    let out = git_cmd(&["hash-object", "-w", "--literally", "-t", kind, "--stdin"])
        .in_dir(dir)
        .stdin(data)
        .suc();
    out.stdout.trim().parse().expect("oid")
}

struct GitRepo {
    _dir: tempfile::TempDir,
    wt: std::path::PathBuf,
}

impl GitRepo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let wt = dir.path().join("w");
        std::fs::create_dir(&wt).expect("mkdir");
        git(&wt, &["init", "-q", "-b", "main"]);
        git(&wt, &["config", "user.name", "A"]);
        git(&wt, &["config", "user.email", "a@example.com"]);
        std::fs::write(wt.join("tracked.txt"), "tracked\n").expect("write");
        git(&wt, &["add", "tracked.txt"]);
        commit_at(&wt, "first", 100);
        git(&wt, &["branch", "side"]);
        std::fs::write(wt.join("second.txt"), "second\n").expect("write");
        git(&wt, &["add", "second.txt"]);
        commit_at(&wt, "second", 200);
        git_cmd(&["tag", "-a", "v1", "-m", "v1", "main~1"])
            .in_dir(&wt)
            .env("GIT_COMMITTER_DATE", "1700000300 +0000")
            .suc();
        git(&wt, &["checkout", "-q", "side"]);
        std::fs::write(wt.join("side.txt"), "side\n").expect("write");
        git(&wt, &["add", "side.txt"]);
        commit_at(&wt, "side", 400);
        git(&wt, &["checkout", "-q", "main"]);
        Self { _dir: dir, wt }
    }

    fn open(&self) -> Repository {
        Repository::discover(Some(&self.wt), &RepositoryOptions::empty()).expect("open")
    }
}

#[test]
fn peel_options_match_git_peel_suffixes() {
    let fixture = GitRepo::new();
    let wt = &fixture.wt;
    let repo = fixture.open();
    let walk = repo.revwalk();
    let cases: [(Peel, &str); 3] = [
        (Peel::Commit, "commit"),
        (Peel::Tree, "tree"),
        (Peel::Blob, "blob"),
    ];
    let specs = [
        "HEAD",
        "main~1",
        "v1",
        "side",
        "HEAD:tracked.txt",
        "v1:tracked.txt",
    ];
    for (peel, suffix) in cases {
        for index_dwim in [IndexDwim::On, IndexDwim::Off] {
            let options = ResolveOptions::default()
                .with_peel(peel)
                .with_index_dwim(index_dwim);
            for spec in specs {
                // Git reads everything after `rev:` as the path, so peel those by type.
                let git_spec = format!("{spec}^{{{suffix}}}");
                let expected = if spec.contains(':') {
                    git_oid(wt, &["rev-parse", "--verify", "-q", spec])
                        .filter(|_| git(wt, &["cat-file", "-t", spec]).trim() == suffix)
                } else {
                    git_oid(wt, &["rev-parse", "--verify", "-q", &git_spec])
                };
                let got = walk.resolve(spec, &options).ok();
                assert_eq!(
                    got, expected,
                    "{spec} with {peel:?}/{index_dwim:?} vs git {git_spec}"
                );
            }
        }
    }
}

#[test]
fn unpeeled_resolution_matches_git_verify() {
    let fixture = GitRepo::new();
    let wt = &fixture.wt;
    let repo = fixture.open();
    let walk = repo.revwalk();
    let verify = ResolveOptions::default()
        .with_index_dwim(IndexDwim::Off)
        .with_purpose(ResolvePurpose::RangeEnd);
    for spec in [
        "HEAD",
        "main~1",
        "v1",
        "side^0",
        ":tracked.txt",
        "HEAD:tracked.txt",
        "main:",
        "tracked.txt",
        "no-such-name",
    ] {
        let expected = git_oid(wt, &["rev-parse", "--verify", "-q", spec]);
        for options in [ResolveOptions::default(), verify] {
            assert_eq!(
                walk.resolve(spec, &options).ok(),
                expected,
                "{spec} {options:?}"
            );
        }
    }
}

#[test]
fn commit_ranges_match_git() {
    let fixture = GitRepo::new();
    let wt = &fixture.wt;
    let repo = fixture.open();
    let walk = repo.revwalk();
    let commit = ResolveOptions::default().with_peel(Peel::Commit);
    let base = git_oid(wt, &["merge-base", "main", "side"]);
    assert_eq!(walk.resolve("main...side", &commit).ok(), base);
    let main = walk.resolve("main", &commit).expect("main");
    let side = walk.resolve("side", &commit).expect("side");
    assert_eq!(
        walk.merge_bases(&[main, side], MergeBaseMode::All)
            .expect("bases"),
        base.into_iter().collect::<Vec<_>>()
    );
    assert_eq!(
        walk.resolve("main~1..main", &commit).ok(),
        git_oid(wt, &["rev-parse", "main"])
    );
    assert_eq!(
        walk.resolve("main..side", &commit).ok(),
        git_oid(wt, &["rev-parse", "side"])
    );
}

#[test]
fn ambiguous_abbreviations_match_git_disambiguation() {
    let fixture = GitRepo::new();
    let wt = &fixture.wt;
    let repo_before = fixture.open();
    let head = repo_before
        .revwalk()
        .resolve("HEAD", &ResolveOptions::default())
        .expect("head");
    let blob = write_with_git(
        wt,
        "blob",
        &colliding(ObjectKind::Blob, head, |n| {
            format!("blob {n}\n").into_bytes()
        }),
    );
    let tracked = git_oid(wt, &["rev-parse", "HEAD:tracked.txt"]).expect("tracked");
    let tree_target = write_with_git(wt, "blob", b"tree target\n");
    let tree = write_with_git(
        wt,
        "tree",
        &colliding(ObjectKind::Tree, tree_target, |n| {
            let mut data = format!("100644 f{n}\0").into_bytes();
            data.extend_from_slice(tracked.as_bytes());
            data
        }),
    );
    let commit_prefix = &head.to_hex()[..4];
    let tree_prefix = &tree_target.to_hex()[..4];
    assert_ne!(blob, tree);
    assert_ne!(
        commit_prefix, tree_prefix,
        "the fixture needs two separate collisions"
    );

    let repo = fixture.open();
    let walk = repo.revwalk();
    let commit = ResolveOptions::default().with_peel(Peel::Commit);
    assert_eq!(walk.resolve(commit_prefix, &commit).ok(), Some(head));
    assert_eq!(
        Some(head),
        git_oid(
            wt,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{commit_prefix}^{{commit}}")
            ]
        )
    );
    let peel_tree = ResolveOptions::default().with_peel(Peel::Tree);
    assert_eq!(walk.resolve(tree_prefix, &peel_tree).ok(), Some(tree));
    assert_eq!(
        Some(tree),
        git_oid(
            wt,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{tree_prefix}^{{tree}}")
            ]
        )
    );
    assert!(walk
        .resolve(commit_prefix, &ResolveOptions::default())
        .is_err());
    assert!(git_oid(wt, &["rev-parse", "--verify", "-q", commit_prefix]).is_none());

    git(wt, &["fsck", "--no-dangling"]);
}
