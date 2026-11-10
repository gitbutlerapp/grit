//! Unfiltered commit counts (`RevWalk::count`, `rev_list` with `count`) vs `git rev-list --count`
//! on repositories without a commit-graph or bitmap.
//!
//! Those counts parse only each commit's parent list: commits stored whole in the single local
//! pack are inflated (and checksum-verified) into a reused buffer, and everything else
//! (deltified, loose) goes through the object database. Grafts, shallow boundaries and
//! replacement refs must change the count exactly as they change Git's, and a corrupt commit
//! entry must fail the count as it fails Git's.

use grit_lib::progress::{CancelToken, NullProgress};

use std::path::{Path, PathBuf};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::revwalk::RevWalkListOptions;
use grit_test_support::{git, unique_tmp, write_file};

fn open(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    Repository::discover(Some(wt), &RepositoryOptions::with_environment(env)).expect("discover")
}

fn init(tag: &str, object_format: &str) -> PathBuf {
    let wt = unique_tmp("rev-list-count", tag);
    git(
        &wt,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={object_format}"),
            ".",
        ],
    );
    git(&wt, &["config", "user.name", "T"]);
    git(&wt, &["config", "user.email", "t@example.com"]);
    wt
}

fn commit(wt: &Path, file: &str, body: &str, msg: &str) {
    write_file(wt, file, body);
    git(wt, &["add", file]);
    git(wt, &["commit", "-qm", msg]);
}

/// History with plain merges, a five-parent octopus merge, commits whose long shared message
/// lets `git repack` store them as deltas, and loose commits on top of the pack.
fn merge_heavy_repo(tag: &str, object_format: &str) -> PathBuf {
    let wt = init(tag, object_format);
    let body = "Shared commit message body that repeats so commits delta well.\n".repeat(40);
    for i in 0..12 {
        commit(
            &wt,
            "base.txt",
            &format!("{i}\n"),
            &format!("base {i}\n\n{body}"),
        );
    }
    for side in 0..3 {
        git(&wt, &["switch", "-qc", &format!("side{side}"), "main~4"]);
        for i in 0..3 {
            commit(
                &wt,
                &format!("side{side}.txt"),
                &format!("{i}\n"),
                &format!("side {side} {i}\n\n{body}"),
            );
        }
        git(&wt, &["switch", "-q", "main"]);
        git(&wt, &["merge", "-q", "--no-edit", &format!("side{side}")]);
    }
    for leg in 0..4 {
        git(&wt, &["switch", "-qc", &format!("leg{leg}"), "main~2"]);
        commit(&wt, &format!("leg{leg}.txt"), "x\n", &format!("leg {leg}"));
    }
    git(&wt, &["switch", "-q", "main"]);
    git(
        &wt,
        &["merge", "-q", "--no-edit", "leg0", "leg1", "leg2", "leg3"],
    );
    git(
        &wt,
        &[
            "repack",
            "-q",
            "-a",
            "-d",
            "-f",
            "--window=50",
            "--depth=50",
        ],
    );
    for i in 0..3 {
        commit(&wt, "loose.txt", &format!("{i}\n"), &format!("loose {i}"));
    }
    git(&wt, &["switch", "-qc", "loose-side", "main~1"]);
    commit(&wt, "loose-side.txt", "1\n", "loose side");
    git(&wt, &["switch", "-q", "main"]);
    git(&wt, &["merge", "-q", "--no-edit", "loose-side"]);
    wt
}

fn git_count(wt: &Path, specs: &[&str]) -> usize {
    let mut args = vec!["rev-list", "--count"];
    args.extend_from_slice(specs);
    git(wt, &args).trim().parse().expect("git count")
}

fn assert_counts_match_git(wt: &Path, specs: &[&str]) {
    let repo = open(wt);
    let expected = git_count(wt, specs);
    let counted = repo
        .revwalk()
        .count(specs, &RevWalkListOptions::default())
        .unwrap_or_else(|e| panic!("count({specs:?}): {e}"));
    assert_eq!(counted, expected, "RevWalk::count({specs:?}) vs git");
    let positive: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
    let listed = repo
        .revwalk()
        .rev_list(
            &positive,
            &[],
            &RevListOptions {
                count: true,
                ..RevListOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_or_else(|e| panic!("rev_list({specs:?}) count: {e}"))
        .commit_count();
    assert_eq!(listed, expected, "rev_list({specs:?}) count vs git");
}

fn assert_no_commit_graph(wt: &Path) {
    let info = wt.join(".git/objects/info");
    assert!(!info.join("commit-graph").exists());
    assert!(!info.join("commit-graphs").exists());
}

fn deltified_commits(wt: &Path) -> usize {
    let pack_dir = wt.join(".git/objects/pack");
    let idx = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("one pack index");
    git(
        wt,
        &["verify-pack", "-v", idx.to_str().expect("utf-8 path")],
    )
    .lines()
    .filter(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.get(1) == Some(&"commit") && fields.len() >= 7
    })
    .count()
}

#[test]
fn count_matches_git_across_merges_octopus_deltas_and_loose_commits() {
    let wt = merge_heavy_repo("sha1", "sha1");
    assert_no_commit_graph(&wt);
    assert!(
        deltified_commits(&wt) > 0,
        "fixture should store some commits as deltas"
    );
    assert_counts_match_git(&wt, &["HEAD"]);
    assert_counts_match_git(&wt, &["main~1"]);
    assert_counts_match_git(&wt, &["side0", "side1", "leg3"]);
    assert_counts_match_git(&wt, &["HEAD", "HEAD", "main"]);
}

#[test]
fn count_matches_git_when_commits_postdate_the_commit_graph() {
    let wt = merge_heavy_repo("stale-graph", "sha1");
    git(&wt, &["commit-graph", "write", "--reachable"]);
    for i in 0..3 {
        commit(
            &wt,
            "after-graph.txt",
            &format!("{i}\n"),
            &format!("after {i}"),
        );
    }
    git(&wt, &["switch", "-qc", "after-side", "main~5"]);
    commit(&wt, "after-side.txt", "1\n", "after side");
    git(&wt, &["switch", "-q", "main"]);
    git(&wt, &["merge", "-q", "--no-edit", "after-side"]);
    assert_counts_match_git(&wt, &["HEAD"]);
    assert_counts_match_git(&wt, &["HEAD~1", "after-side"]);
}

#[test]
fn count_matches_git_in_sha256_repository() {
    let wt = merge_heavy_repo("sha256", "sha256");
    assert_no_commit_graph(&wt);
    assert_counts_match_git(&wt, &["HEAD"]);
    assert_counts_match_git(&wt, &["side2", "leg0"]);
}

#[test]
fn count_honours_grafts_like_git() {
    let wt = merge_heavy_repo("grafts", "sha1");
    let cut = git(&wt, &["rev-parse", "main~3"]).trim().to_owned();
    let new_parent = git(&wt, &["rev-parse", "side0~2"]).trim().to_owned();
    let root_like = git(&wt, &["rev-parse", "side1"]).trim().to_owned();
    std::fs::write(
        wt.join(".git/info/grafts"),
        format!("{cut} {new_parent}\n{root_like}\n"),
    )
    .expect("write grafts");
    assert_counts_match_git(&wt, &["HEAD"]);
    assert_counts_match_git(&wt, &["side1"]);
}

#[test]
fn count_stops_at_shallow_boundaries_like_git() {
    let src = merge_heavy_repo("shallow-src", "sha1");
    let parent = unique_tmp("rev-list-count", "shallow-dst");
    let url = format!("file://{}", src.display());
    git(&parent, &["clone", "-q", "--depth", "5", &url, "dst"]);
    let dst = parent.join("dst");
    assert!(dst.join(".git/shallow").exists());
    assert_no_commit_graph(&dst);
    assert_counts_match_git(&dst, &["HEAD"]);
}

#[test]
fn count_follows_replacement_refs_like_git() {
    let wt = merge_heavy_repo("replace", "sha1");
    let replaced = git(&wt, &["rev-parse", "main~2"]).trim().to_owned();
    let replacement = git(&wt, &["rev-parse", "main~8"]).trim().to_owned();
    git(&wt, &["replace", &replaced, &replacement]);
    assert_ne!(
        git_count(&wt, &["HEAD"]),
        git(
            &wt,
            &["--no-replace-objects", "rev-list", "--count", "HEAD"]
        )
        .trim()
        .parse::<usize>()
        .expect("count"),
        "the replacement should change the count"
    );
    assert_counts_match_git(&wt, &["HEAD"]);
}

#[test]
fn count_fails_like_git_on_a_bad_zlib_checksum_after_the_headers() {
    let wt = init("corrupt", "sha1");
    commit(&wt, "f.txt", "f\n", "only commit");
    git(&wt, &["repack", "-q", "-a", "-d", "-f"]);
    let head = git(&wt, &["rev-parse", "HEAD"]).trim().to_owned();
    let pack_dir = wt.join(".git/objects/pack");
    let idx = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack index");
    let listing = git(
        &wt,
        &["verify-pack", "-v", idx.to_str().expect("utf-8 path")],
    );
    let (offset, in_pack) = listing
        .lines()
        .find_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            (f.first() == Some(&head.as_str())).then(|| {
                (
                    f[4].parse::<usize>().expect("offset"),
                    f[3].parse::<usize>().expect("size in pack"),
                )
            })
        })
        .expect("commit entry");
    // The entry's last byte is part of the zlib adler32 checksum, after every commit header.
    let pack = idx.with_extension("pack");
    let mut bytes = std::fs::read(&pack).expect("read pack");
    bytes[offset + in_pack - 1] ^= 0x01;
    let mut perms = std::fs::metadata(&pack).expect("stat pack").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&pack, perms).expect("make pack writable");
    std::fs::write(&pack, &bytes).expect("write pack");

    let git_status = grit_test_support::git_cmd(&["rev-list", "--count", "HEAD"])
        .in_dir(&wt)
        .exec();
    assert!(!git_status.ok(), "git must reject the corrupt commit");
    let repo = open(&wt);
    assert!(
        repo.revwalk()
            .count(
                &["HEAD"],
                &RevWalkListOptions::default(),
                &mut NullProgress,
                &CancelToken::never()
            )
            .is_err(),
        "RevWalk::count must reject the corrupt commit"
    );
    assert!(
        repo.revwalk()
            .rev_list(
                &["HEAD".to_owned()],
                &[],
                &RevListOptions {
                    count: true,
                    ..RevListOptions::default()
                },
                &mut NullProgress,
                &CancelToken::never()
            )
            .is_err(),
        "rev_list count must reject the corrupt commit"
    );
}
