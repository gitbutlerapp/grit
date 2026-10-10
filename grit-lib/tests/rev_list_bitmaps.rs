//! `rev-list` bitmap acceleration and commit-graph count fast paths.

use std::collections::HashSet;
use std::process::Command;

use grit_lib::hot_path_test_metrics::HotPathMetricsScope;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, ObjectFilter, OrderingMode, RevListOptions};
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
    assert!(out.status.success(), "git {args:?} failed");
    std::str::from_utf8(&out.stdout).unwrap().trim().to_string()
}

fn git_count(repo: &RepoFixture, extra: &[&str]) -> usize {
    let mut args = vec!["rev-list", "--count"];
    args.extend_from_slice(extra);
    let out = Command::new("git")
        .current_dir(repo.path())
        .args(&args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git rev-list --count: {:?}", extra);
    std::str::from_utf8(&out.stdout)
        .unwrap()
        .trim()
        .parse()
        .expect("count")
}

fn open_repo(path: &std::path::Path) -> Repository {
    Repository::discover(Some(path)).expect("open repo")
}

fn build_history(repo: &RepoFixture) {
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
}

fn repack_with_bitmap(repo: &RepoFixture) {
    git_ok(repo, &["repack", "-adb"]);
}

fn write_commit_graph(repo: &RepoFixture) {
    let out = Command::new("git")
        .current_dir(repo.path())
        .args(["commit-graph", "write", "--reachable", "--changed-paths"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git commit-graph write");
    assert!(
        out.status.success(),
        "commit-graph write: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn grit_count(repo: &Repository, opts: RevListOptions) -> usize {
    let result = rev_list(repo, &[], &[], &opts).expect("rev-list");
    if opts.count && opts.objects {
        result.count_with_objects_total()
    } else {
        result.commit_count()
    }
}

fn grit_object_set(repo: &Repository, opts: RevListOptions) -> HashSet<ObjectId> {
    let result = rev_list(repo, &[], &[], &opts).expect("rev-list");
    let mut set = HashSet::new();
    set.extend(result.commits);
    set.extend(result.objects.iter().map(|(o, _)| *o));
    set
}

#[test]
fn count_all_matches_git() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    repack_with_bitmap(&fixture);
    write_commit_graph(&fixture);
    let repo = open_repo(fixture.path());
    let git_n = git_count(&fixture, &["--all"]);
    let opts = RevListOptions {
        all_refs: true,
        count: true,
        use_commit_graph: true,
        ..Default::default()
    };
    assert_eq!(grit_count(&repo, opts), git_n);
}

#[test]
fn count_objects_matches_git_use_bitmap_index() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    repack_with_bitmap(&fixture);
    write_commit_graph(&fixture);
    let repo = open_repo(fixture.path());
    let git_n = git_count(&fixture, &["--objects", "--all", "--use-bitmap-index"]);
    let opts = RevListOptions {
        all_refs: true,
        count: true,
        objects: true,
        use_bitmap_index: true,
        use_commit_graph: true,
        ..Default::default()
    };
    assert_eq!(grit_count(&repo, opts.clone()), git_n);
    let listed = rev_list(&repo, &[], &[], &opts).expect("rev-list");
    assert!(listed.bitmap_object_format);
}

#[test]
fn count_objects_auto_bitmap_without_flag() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let git_n = git_count(&fixture, &["--objects", "--all", "--use-bitmap-index"]);
    let opts = RevListOptions {
        all_refs: true,
        count: true,
        objects: true,
        use_commit_graph: true,
        ..Default::default()
    };
    assert_eq!(grit_count(&repo, opts), git_n);
}

#[test]
fn objects_listing_with_bitmap_is_same_set_as_walk() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let base = RevListOptions {
        all_refs: true,
        objects: true,
        use_commit_graph: true,
        ..Default::default()
    };
    let walk = grit_object_set(&repo, base.clone());
    let mut bitmap = base;
    bitmap.use_bitmap_index = true;
    let via_bitmap = grit_object_set(&repo, bitmap);
    assert_eq!(walk, via_bitmap);
}

#[test]
fn incompatible_options_fall_back() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let baseline = RevListOptions {
        all_refs: true,
        count: true,
        objects: true,
        use_bitmap_index: true,
        use_commit_graph: true,
        ..Default::default()
    };

    let cases: Vec<RevListOptions> = vec![
        RevListOptions {
            max_count: Some(2),
            ..baseline.clone()
        },
        RevListOptions {
            skip: 1,
            ..baseline.clone()
        },
        RevListOptions {
            paths: vec!["readme".into()],
            ..baseline.clone()
        },
        RevListOptions {
            first_parent: true,
            ..baseline.clone()
        },
        RevListOptions {
            boundary: true,
            ..baseline.clone()
        },
        RevListOptions {
            ordering: OrderingMode::Topo,
            ..baseline.clone()
        },
        RevListOptions {
            reverse: true,
            ..baseline.clone()
        },
        RevListOptions {
            filter: Some(ObjectFilter::TreeDepth(1)),
            ..baseline.clone()
        },
    ];

    for opts in cases {
        let result = rev_list(&repo, &[], &[], &opts).expect("rev-list");
        assert!(
            !result.bitmap_object_format,
            "expected fallback for incompatible rev-list options"
        );
        assert!(result.count_with_objects_total() > 0);
    }
}

fn git_rev_list_object_set(
    fixture: &RepoFixture,
    positive: &[&str],
    negative: &[&str],
    use_bitmap: bool,
) -> HashSet<ObjectId> {
    let mut args: Vec<String> = vec!["rev-list".into(), "--objects".into()];
    if use_bitmap {
        args.push("--use-bitmap-index".into());
    }
    for p in positive {
        args.push((*p).to_string());
    }
    for n in negative {
        args.push(if n.starts_with('^') {
            (*n).to_string()
        } else {
            format!("^{n}")
        });
    }
    let out = Command::new("git")
        .current_dir(fixture.path())
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
            let token = std::str::from_utf8(l)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap();
            ObjectId::from_hex(token).expect("oid")
        })
        .collect()
}

fn grit_rev_list_object_set(
    repo: &Repository,
    positive: &[&str],
    negative: &[&str],
    use_bitmap: bool,
) -> (HashSet<ObjectId>, bool) {
    let pos: Vec<String> = positive.iter().map(|s| (*s).to_string()).collect();
    let neg: Vec<String> = negative
        .iter()
        .map(|s| s.strip_prefix('^').unwrap_or(s).to_string())
        .collect();
    let opts = RevListOptions {
        objects: true,
        use_bitmap_index: use_bitmap,
        use_commit_graph: true,
        ..Default::default()
    };
    let result = rev_list(repo, &pos, &neg, &opts).expect("rev-list");
    let mut set = HashSet::new();
    set.extend(result.commits);
    set.extend(result.objects.iter().map(|(o, _)| *o));
    (set, result.bitmap_object_format)
}

#[test]
fn bitmap_honors_negative_blob_root() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let head = "HEAD";
    let blob = git_stdout(&fixture, &["rev-parse", "HEAD:readme"]);
    let git_set = git_rev_list_object_set(&fixture, &[head], &[&blob], true);
    let (grit_set, bitmap) = grit_rev_list_object_set(&repo, &[head], &[&blob], true);
    assert!(bitmap, "expected bitmap acceleration");
    assert!(!git_set.contains(&ObjectId::from_hex(&blob).unwrap()));
    assert_eq!(grit_set, git_set);
}

#[test]
fn bitmap_honors_negative_tree_root() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    repack_with_bitmap(&fixture);
    let repo = open_repo(fixture.path());
    let head = "HEAD";
    let tree = git_stdout(&fixture, &["rev-parse", "HEAD^{tree}"]);
    let git_set = git_rev_list_object_set(&fixture, &[head], &[&tree], true);
    let (grit_set, bitmap) = grit_rev_list_object_set(&repo, &[head], &[&tree], true);
    assert!(bitmap, "expected bitmap acceleration");
    assert_eq!(grit_set, git_set);
}

#[test]
fn no_bitmap_count_uses_commit_graph() {
    let fixture = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fixture);
    git_ok(&fixture, &["repack", "-ad"]);
    write_commit_graph(&fixture);
    let has_bitmap = fixture
        .path()
        .join(".git/objects/pack")
        .read_dir()
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|ext| ext == "bitmap"));
    assert!(!has_bitmap, "fixture should have no pack bitmap");
    let repo = open_repo(fixture.path());
    let metrics = repo.odb.hot_path_metrics_arc();
    metrics.reset_commit_parse_calls();
    metrics.set_commit_parse_counting(true);
    let _scope = HotPathMetricsScope::install(metrics);
    let git_n = git_count(&fixture, &["--all"]);
    let opts = RevListOptions {
        all_refs: true,
        count: true,
        use_commit_graph: true,
        ..Default::default()
    };
    assert_eq!(grit_count(&repo, opts), git_n);
    assert_eq!(
        repo.odb.hot_path_metrics().commit_parse_calls(),
        0,
        "commit-graph count path must not parse commit objects"
    );
}
