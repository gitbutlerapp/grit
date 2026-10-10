//! Bitmap reachability engine vs object walk and system Git.

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use grit_lib::bitmap_walk::{BitmapWalkError, ReachabilityQuery, ReachableSet};
use grit_lib::objects::ObjectId;
use grit_lib::pack_bitmap::BitmapIndex;
use grit_lib::pack_rev::append_hashfile_checksum;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, MissingAction, ObjectFilter, RevListOptions};
use grit_test_support::{HashAlgo, RepoFixture};

fn git_ok(repo: &RepoFixture, args: &[&str]) {
    let out = repo.git(args);
    assert!(out.ok, "git {:?} failed: {}", args, out.stderr);
}

fn git_stdout(repo: &RepoFixture, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo.path())
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn open_repo(path: &std::path::Path) -> Repository {
    Repository::discover(Some(path)).expect("open repo")
}

fn all_commits(repo: &Repository) -> Vec<ObjectId> {
    let wt = repo.work_tree.as_ref().unwrap_or(&repo.git_dir);
    let out = Command::new("git")
        .current_dir(wt)
        .args(["rev-list", "--all"])
        .output()
        .expect("rev-list");
    assert!(out.status.success());
    out.stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| ObjectId::from_hex(std::str::from_utf8(l).unwrap().trim()).unwrap())
        .collect()
}

fn grit_object_set(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: Option<&ObjectFilter>,
) -> HashSet<ObjectId> {
    let wants: Vec<String> = wants.iter().map(|o| o.to_hex()).collect();
    let haves: Vec<String> = haves.iter().map(|o| o.to_hex()).collect();
    let opts = RevListOptions {
        objects: true,
        no_object_names: true,
        quiet: true,
        filter: filter.cloned(),
        ..Default::default()
    };
    let result = rev_list(repo, &wants, &haves, &opts).expect("rev-list");
    let mut set = HashSet::new();
    for oid in &result.commits {
        set.insert(*oid);
    }
    for (oid, _) in &result.objects {
        set.insert(*oid);
    }
    set
}

fn git_rev_list_objects(
    repo: &RepoFixture,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: Option<&str>,
    use_bitmap: bool,
) -> HashSet<ObjectId> {
    let mut args: Vec<String> = vec!["rev-list".into(), "--objects".into()];
    if use_bitmap {
        args.push("--use-bitmap-index".into());
    }
    if let Some(f) = filter {
        args.push(format!("--filter={f}"));
    }
    for w in wants {
        args.push(w.to_hex());
    }
    for h in haves {
        args.push(format!("^{}", h.to_hex()));
    }
    let out = Command::new("git")
        .current_dir(repo.path())
        .args(&args)
        .output()
        .expect("git rev-list");
    assert!(
        out.status.success(),
        "git rev-list: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let line = std::str::from_utf8(l).unwrap();
            let token = line.split_whitespace().next().unwrap_or(line);
            ObjectId::from_hex(token).expect("oid")
        })
        .collect()
}

fn bitmap_query(
    repo: &Repository,
    index: &Arc<BitmapIndex>,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: Option<&ObjectFilter>,
) -> Result<ReachableSet, BitmapWalkError> {
    index.reachability(
        repo,
        ReachabilityQuery {
            wants,
            haves,
            filter,
        },
        MissingAction::Error,
    )
}

fn build_rich_history(repo: &RepoFixture) {
    git_ok(repo, &["checkout", "-b", "main"]);
    std::fs::write(repo.path().join("readme"), b"v1").unwrap();
    git_ok(repo, &["add", "readme"]);
    git_ok(repo, &["commit", "-m", "root"]);
    std::fs::write(repo.path().join("readme"), b"v2").unwrap();
    git_ok(repo, &["commit", "-am", "second"]);
    git_ok(repo, &["checkout", "-b", "side"]);
    std::fs::write(repo.path().join("side.txt"), b"side").unwrap();
    git_ok(repo, &["add", "side.txt"]);
    git_ok(repo, &["commit", "-m", "side"]);
    git_ok(repo, &["checkout", "main"]);
    git_ok(repo, &["merge", "--no-ff", "side", "-m", "merge side"]);
    git_ok(repo, &["tag", "-a", "annotated", "-m", "anno"]);
}

fn repack_with_bitmap(repo: &RepoFixture) {
    git_ok(repo, &["repack", "-adb"]);
}

fn add_post_repack_commits(repo: &RepoFixture) {
    std::fs::write(repo.path().join("after.txt"), b"after bitmap repack").unwrap();
    git_ok(repo, &["add", "after.txt"]);
    git_ok(repo, &["commit", "-m", "after repack loose"]);
    std::fs::write(repo.path().join("after2.txt"), b"second pack").unwrap();
    git_ok(repo, &["add", "after2.txt"]);
    git_ok(repo, &["commit", "-m", "after repack second pack"]);
}

fn pick_subset(commits: &[ObjectId], seed: u64, count: usize) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let n = commits.len();
    if n == 0 {
        return out;
    }
    let mut x = seed;
    for i in 0..count {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        let idx = (x as usize) % n;
        let pick = commits[(idx + i) % n];
        if !out.contains(&pick) {
            out.push(pick);
        }
    }
    out
}

fn assert_sets_eq(label: &str, got: &HashSet<ObjectId>, expected: &HashSet<ObjectId>) {
    if got != expected {
        let mut only_got: Vec<_> = got.difference(expected).copied().collect();
        let mut only_exp: Vec<_> = expected.difference(got).copied().collect();
        only_got.sort();
        only_exp.sort();
        panic!(
            "{label}: mismatch ({} vs {}); only got {:?}; only expected {:?}",
            got.len(),
            expected.len(),
            only_got.iter().take(8).collect::<Vec<_>>(),
            only_exp.iter().take(8).collect::<Vec<_>>()
        );
    }
}

#[test]
fn head_reachability_includes_pack_objects() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    add_post_repack_commits(&fixture);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).unwrap();
    let got = bitmap_query(&repo, &index, &[head], &[], None)
        .expect("query")
        .object_ids();
    assert!(
        got.len() > 10,
        "HEAD reachability should include pack objects, got {}",
        got.len()
    );
}

#[test]
fn bitmap_query_matches_object_walk() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    add_post_repack_commits(&fixture);

    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let commits = all_commits(&repo);
    assert!(commits.len() > 4);

    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).unwrap();
    for seed in 1u64..8 {
        let wants = [head];
        let haves = if seed >= 5 {
            pick_subset(&commits, seed.wrapping_add(99), 2)
        } else {
            Vec::new()
        };
        let grit_ref = grit_object_set(&repo, &wants, &haves, None);
        let got = bitmap_query(&repo, &index, &wants, &haves, None)
            .expect("query")
            .object_ids();
        let git_norm = git_rev_list_objects(&fixture, &wants, &haves, None, false);
        let git_set = git_rev_list_objects(&fixture, &wants, &haves, None, true);
        assert_sets_eq("git normal", &git_norm, &git_set);
        assert_sets_eq("git bitmap", &got, &git_set);
        if haves.is_empty() {
            assert_sets_eq("grit rev-list vs git", &grit_ref, &git_set);
            assert_sets_eq("grit walk", &got, &grit_ref);
        }
    }
}

#[test]
fn filters_match_non_bitmap_walk() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);

    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).unwrap();
    let wants = [head];
    let haves: [ObjectId; 0] = [];

    let cases: &[(&str, ObjectFilter)] = &[
        ("blob:none", ObjectFilter::BlobNone),
        ("blob:limit=1", ObjectFilter::BlobLimit(1)),
        (
            "object:type=commit",
            ObjectFilter::ObjectType(grit_lib::rev_list::FilterObjectKind::Commit),
        ),
        ("tree:0", ObjectFilter::TreeDepth(0)),
    ];

    for (spec, filter) in cases {
        let grit_ref = grit_object_set(&repo, &wants, &haves, Some(filter));
        let got = bitmap_query(&repo, &index, &wants, &haves, Some(filter))
            .expect("query")
            .object_ids();
        assert_sets_eq(spec, &got, &grit_ref);
        let git_set = git_rev_list_objects(&fixture, &wants, &haves, Some(spec), true);
        assert_sets_eq(&format!("git {spec}"), &got, &git_set);
    }
}

#[test]
fn tree_zero_keeps_loose_commits_after_repack() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    git_ok(
        &fixture,
        &["commit", "--allow-empty", "-m", "loose after bitmap repack"],
    );
    add_post_repack_commits(&fixture);

    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).unwrap();
    let filter = ObjectFilter::TreeDepth(0);
    let wants = [head];
    let haves: [ObjectId; 0] = [];

    let git_set = git_rev_list_objects(&fixture, &wants, &haves, Some("tree:0"), true);
    let got = bitmap_query(&repo, &index, &wants, &haves, Some(&filter))
        .expect("query")
        .object_ids();
    assert_sets_eq("tree:0 git", &got, &git_set);

    let commit_count = git_set
        .iter()
        .filter(|oid| {
            repo.odb
                .read_info(oid)
                .map(|i| i.kind == grit_lib::objects::ObjectKind::Commit)
                .unwrap_or(false)
        })
        .count();
    assert!(
        commit_count >= 2,
        "tree:0 should retain multiple commits (packed + loose), got {commit_count}"
    );
}

#[test]
fn reachability_skips_missing_gitlink() {
    const MISSING_GITLINK: &str = "855827c583bc30645ba427885caa40c5b81764d2";
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    let tree = {
        let mut child = Command::new("git")
            .current_dir(fixture.path())
            .args(["mktree"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("mktree");
        writeln!(
            child.stdin.as_mut().expect("stdin"),
            "160000 commit {MISSING_GITLINK}\tsub"
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
    let parent = git_stdout(&fixture, &["rev-parse", "HEAD"]);
    let commit = git_stdout(
        &fixture,
        &["commit-tree", &tree, "-p", &parent, "-m", "add gitlink"],
    );
    git_ok(&fixture, &["update-ref", "refs/heads/main", &commit]);
    repack_with_bitmap(&fixture);

    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&commit).unwrap();
    let got = bitmap_query(&repo, &index, &[head], &[], None).expect("query");
    assert!(
        got.contains(&head),
        "HEAD commit should be reachable when gitlink object is absent"
    );
}

#[test]
fn unsupported_filter_and_shallow_fall_back() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).unwrap();

    let sparse = ObjectFilter::SparseOid("HEAD:readme".to_owned());
    assert!(matches!(
        bitmap_query(&repo, &index, &[head], &[], Some(&sparse)),
        Err(BitmapWalkError::Unsupported(_))
    ));

    let tree1 = ObjectFilter::TreeDepth(1);
    assert!(matches!(
        bitmap_query(&repo, &index, &[head], &[], Some(&tree1)),
        Err(BitmapWalkError::Unsupported(_))
    ));

    let boundary = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD~1"])).unwrap();
    std::fs::write(
        fixture.path().join(".git/shallow"),
        format!("{}\n", boundary.to_hex()),
    )
    .unwrap();
    let shallow_repo = open_repo(fixture.path());
    let shallow_index = BitmapIndex::open(&shallow_repo)
        .expect("open")
        .expect("bitmap on shallow-marked repo");
    use grit_lib::rev_list::shallow_boundary_oids;
    assert!(!shallow_boundary_oids(&shallow_repo.git_dir).is_empty());
    assert!(matches!(
        bitmap_query(&shallow_repo, &shallow_index, &[head], &[], None),
        Err(BitmapWalkError::Unsupported(_))
    ));
}

#[test]
fn verify_commit_detects_tampered_bitmap() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).unwrap();
    assert!(index
        .verify_commit(&repo, &head, MissingAction::Error)
        .unwrap());

    let pack_dir = fixture.objects_dir().join("pack");
    let bitmap_path = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "bitmap"))
        .expect("bitmap");
    let mut perms = std::fs::metadata(&bitmap_path).unwrap().permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(&bitmap_path, perms).unwrap();
    let hash_len = repo.odb.hash_algo().len();
    let mut bytes = std::fs::read(&bitmap_path).unwrap();
    assert!(bytes.len() > hash_len + 16);
    let idx = bytes.len() / 2;
    bytes[idx] ^= 0x40;
    bytes.truncate(bytes.len() - hash_len);
    append_hashfile_checksum(&mut bytes, hash_len);
    std::fs::write(&bitmap_path, &bytes).unwrap();

    let repo2 = open_repo(fixture.path());
    let index2 = BitmapIndex::open(&repo2)
        .expect("open")
        .expect("tampered bitmap file should still open for verify_commit");
    assert!(
        !index2
            .verify_commit(&repo2, &head, MissingAction::Error)
            .unwrap(),
        "tampered bitmap should not verify"
    );
}

#[test]
fn sha256_query() {
    let fixture = RepoFixture::init(HashAlgo::Sha256).expect("init");
    build_rich_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let index = BitmapIndex::open(&repo).expect("open").expect("bitmap");
    let head = ObjectId::from_hex(&git_stdout(&fixture, &["rev-parse", "HEAD"])).unwrap();
    let wants = [head];
    let grit_ref = grit_object_set(&repo, &wants, &[], None);
    let got = bitmap_query(&repo, &index, &wants, &[], None)
        .expect("query")
        .object_ids();
    assert_sets_eq("sha256 grit", &got, &grit_ref);
    let git_set = git_rev_list_objects(&fixture, &wants, &[], None, true);
    assert_sets_eq("sha256 git", &got, &git_set);
}
