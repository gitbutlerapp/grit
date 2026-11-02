//! Path-limited history with `--follow` vs system `git log --follow`.
//!
//! Optional strict fixtures (`GRIT_TEST_GIT_SRC_STRICT=1`) expect a checkout of
//! [git/git](https://github.com/git/git) at commit `50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7`
//! (see `GRIT_TEST_GIT_SRC`).

use crate::support;

use std::path::Path;

use grit_lib::revwalk::RevWalkListOptions;

use support::{git, open_grit_repo, HistoryRepo};

fn grit_follow_commits_with(
    wt: &Path,
    path: &str,
    limit: Option<usize>,
    full_history: bool,
) -> Vec<String> {
    let repo = open_grit_repo(wt);
    repo.rev_walk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![path.to_owned()],
                follow: true,
                full_history,
                limit,
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit follow walk")
        .into_iter()
        .map(|oid| oid.to_hex())
        .collect()
}

fn grit_follow_commits(wt: &Path, path: &str, limit: Option<usize>) -> Vec<String> {
    grit_follow_commits_with(wt, path, limit, false)
}

fn git_follow_commits_with(
    wt: &Path,
    path: &str,
    limit: Option<usize>,
    full_history: bool,
) -> Vec<String> {
    let mut args = vec!["log", "--follow", "--pretty=%H"];
    if full_history {
        args.push("--full-history");
    }
    let limit_s;
    if let Some(n) = limit {
        args.push("-n");
        limit_s = n.to_string();
        args.push(limit_s.as_str());
    }
    args.push("--");
    args.push(path);
    git(wt, &args)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn git_follow_commits(wt: &Path, path: &str, limit: Option<usize>) -> Vec<String> {
    git_follow_commits_with(wt, path, limit, false)
}

fn init_rename_repro(wt: &Path) {
    git(wt, &["init", "-q"]);
    std::fs::write(wt.join("old.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").expect("write");
    git(wt, &["add", "old.txt"]);
    git(wt, &["commit", "-m", "add"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("old.txt"))
        .expect("open")
        .write_all(b"more\n")
        .expect("append");
    git(wt, &["add", "old.txt"]);
    git(wt, &["commit", "-m", "edit1"]);
    git(wt, &["mv", "old.txt", "new.txt"]);
    git(wt, &["commit", "-m", "rename"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("new.txt"))
        .expect("open")
        .write_all(b"more\n")
        .expect("append");
    git(wt, &["add", "new.txt"]);
    git(wt, &["commit", "-m", "edit2"]);
}

use std::io::Write;

/// Issue #2434: `--follow` on a path that no longer exists at HEAD (renamed away).
#[test]
fn follow_old_path_after_rename_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q", "-b", "main"]);
    let initial: String = (1..=40)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    commit_file(wt, "a.txt", &(initial + "\n"), "add a");
    std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("a.txt"))
        .expect("open")
        .write_all(b"41\n")
        .expect("append");
    git(wt, &["add", "a.txt"]);
    git(wt, &["commit", "-m", "edit a"]);
    git(wt, &["mv", "a.txt", "b.txt"]);
    git(wt, &["commit", "-m", "rename a to b"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("b.txt"))
        .expect("open")
        .write_all(b"42\n")
        .expect("append");
    git(wt, &["add", "b.txt"]);
    git(wt, &["commit", "-m", "edit b"]);

    let grit = grit_follow_commits(wt, "a.txt", None);
    let git_oids = git_follow_commits(wt, "a.txt", None);
    assert_eq!(
        grit, git_oids,
        "follow old path after rename must match git"
    );
    assert_eq!(grit.len(), 3, "rename, edit a, add a");
}

#[test]
fn follow_path_after_plain_delete_then_unrelated_commits_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    commit_file(wt, "gone.txt", "v1\n", "add");
    git(wt, &["rm", "gone.txt"]);
    git(wt, &["commit", "-m", "del"]);
    commit_file(wt, "other.txt", "x\n", "unrelated");

    let grit = grit_follow_commits(wt, "gone.txt", None);
    let git_oids = git_follow_commits(wt, "gone.txt", None);
    assert_eq!(grit, git_oids, "follow deleted path must match git");
    assert_eq!(grit.len(), 2, "del and add");
}

#[test]
fn follow_never_existed_path_stays_empty() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    commit_file(wt, "real.txt", "v\n", "add");

    let grit = grit_follow_commits(wt, "missing.txt", None);
    let git_oids = git_follow_commits(wt, "missing.txt", None);
    assert_eq!(grit, git_oids);
    assert!(grit.is_empty(), "path that never existed has no history");
}

#[test]
fn follow_rename_repro_lists_pre_rename_commits() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids, "follow history must match git");

    let subjects: Vec<_> = git(wt, &["log", "--follow", "--oneline", "--", "new.txt"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(subjects.len(), 4, "expected add through edit2");
    assert!(
        subjects.last().unwrap().contains("add"),
        "oldest commit must be add: {:?}",
        subjects
    );
}

#[test]
fn follow_without_flag_stops_at_rename() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);

    let repo = open_grit_repo(wt);
    let oids = repo
        .rev_walk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec!["new.txt".to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("no-follow walk");
    assert_eq!(oids.len(), 2, "rename and edit2 only without follow");
}

#[test]
fn follow_two_rename_chain_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    std::fs::write(wt.join("a.txt"), "v1\n").expect("write");
    git(wt, &["add", "a.txt"]);
    git(wt, &["commit", "-m", "c1"]);
    git(wt, &["mv", "a.txt", "b.txt"]);
    git(wt, &["commit", "-m", "r1"]);
    git(wt, &["mv", "b.txt", "c.txt"]);
    git(wt, &["commit", "-m", "r2"]);

    let grit = grit_follow_commits(wt, "c.txt", None);
    let git_oids = git_follow_commits(wt, "c.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 3);
}

#[test]
fn follow_respects_limit_after_path_switch() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);

    let grit = grit_follow_commits(wt, "new.txt", Some(2));
    let git_oids = git_follow_commits(wt, "new.txt", Some(2));
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 2);
}

#[test]
fn follow_full_history_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);

    let grit = grit_follow_commits_with(wt, "new.txt", None, true);
    let git_oids = git_follow_commits_with(wt, "new.txt", None, true);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 4);
}

#[test]
fn follow_detects_renames_when_diff_renames_is_truthy_numeric() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);
    git(wt, &["config", "diff.renames", "100"]);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 4);
}

#[test]
fn follow_detects_renames_when_diff_renames_disabled() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);
    git(wt, &["config", "diff.renames", "false"]);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 4);
}

fn commit_file(wt: &Path, path: &str, content: &str, msg: &str) {
    std::fs::write(wt.join(path), content).expect("write");
    git(wt, &["add", path]);
    git(wt, &["commit", "-m", msg]);
}

#[test]
fn follow_uses_default_rename_threshold_when_diff_renames_is_numeric() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    commit_file(wt, "old.txt", "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\n", "add");
    git(wt, &["mv", "old.txt", "new.txt"]);
    git(wt, &["commit", "-m", "rename"]);
    git(wt, &["config", "diff.renames", "80"]);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(
        grit.len(),
        2,
        "numeric diff.renames must not act as -M cutoff"
    );
}

#[test]
fn follow_ordinary_delete_readd_matches_git_three_commits() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    commit_file(wt, "f.txt", "v1\n", "add");
    git(wt, &["rm", "f.txt"]);
    git(wt, &["commit", "-m", "del"]);
    commit_file(wt, "f.txt", "v3\n", "readd");

    let grit = grit_follow_commits(wt, "f.txt", None);
    let git_oids = git_follow_commits(wt, "f.txt", None);
    assert_eq!(grit, git_oids, "ordinary delete/re-add must match git");
    assert_eq!(grit.len(), 3, "expected add, del, readd");
}

#[test]
fn follow_delete_readd_and_basename_collision_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    std::fs::create_dir_all(wt.join("dir")).expect("mkdir");
    commit_file(wt, "dir/foo.txt", "v1\n", "c1");
    commit_file(wt, "dir/foo.txt", "v1\nv2\n", "c2");
    commit_file(wt, "foo.txt", "root\n", "root-foo");
    commit_file(wt, "dir/foo.txt", "v1\nv2\nv3\n", "c3");
    git(wt, &["rm", "dir/foo.txt"]);
    git(wt, &["commit", "-m", "del"]);
    std::fs::create_dir_all(wt.join("dir")).expect("mkdir");
    commit_file(wt, "dir/foo.txt", "v4\n", "readd");

    let grit = grit_follow_commits(wt, "dir/foo.txt", None);
    let git_oids = git_follow_commits(wt, "dir/foo.txt", None);
    assert_eq!(
        grit, git_oids,
        "delete/re-add with root basename collision must match git"
    );
    assert_eq!(grit.len(), 5, "expected c1,c2,c3,del,readd");
}

/// Side branch edits the legacy name after mainline renamed into `builtin/` (order-dependent path).
#[test]
fn follow_merge_after_mainline_rename_skips_side_legacy_edits() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(wt.join("builtin")).expect("mkdir");
    commit_file(wt, "builtin/log.c", "v1\n", "add-new-name");
    git(wt, &["branch", "side"]);
    git(wt, &["checkout", "side"]);
    commit_file(wt, "builtin-log.c", "legacy\n", "side-legacy-only");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "builtin/log.c", "v1\nmain\n", "main-edit");
    git(wt, &["merge", "side", "-qm", "merge-side-after"]);

    let grit = grit_follow_commits(wt, "builtin/log.c", None);
    let git_oids = git_follow_commits(wt, "builtin/log.c", None);
    assert_eq!(
        grit, git_oids,
        "follow must not pick up side legacy-path edits merged after the mainline rename"
    );
    assert_eq!(grit.len(), 2, "expected add-new-name and main-edit only");
}

/// Neutral-path regression mirroring parallel merge/rename topology (Git #2459 shape).
#[test]
fn follow_widget_parallel_merge_rename_matches_git_order() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q", "-b", "main"]);
    commit_file(wt, "src-widget.c", "v1\n", "add");
    git(wt, &["branch", "side"]);
    git(wt, &["checkout", "side"]);
    commit_file(wt, "src-widget.c", "v1\nside\n", "side-on-old-name");
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "side", "-qm", "merge-side"]);
    std::fs::create_dir_all(wt.join("src")).expect("mkdir");
    git(wt, &["mv", "src-widget.c", "src/widget.c"]);
    git(wt, &["commit", "-qm", "rename-to-src"]);

    let grit = grit_follow_commits(wt, "src/widget.c", None);
    let git_oids = git_follow_commits(wt, "src/widget.c", None);
    assert_eq!(
        grit, git_oids,
        "follow on src/widget.c must match system git exactly (order and OIDs)"
    );
    assert_eq!(grit.len(), 3, "rename, side-on-old-name, add");
}

/// Side branch edits the file, then merges into main before a later rename (Git #2459 shape).
#[test]
fn follow_merge_before_mainline_rename_lists_side_edits() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q", "-b", "main"]);
    commit_file(wt, "builtin-log.c", "v1\n", "add");
    git(wt, &["branch", "side"]);
    git(wt, &["checkout", "side"]);
    commit_file(wt, "builtin-log.c", "v1\nside\n", "side-on-old-name");
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "side", "-qm", "merge-side"]);
    std::fs::create_dir_all(wt.join("builtin")).expect("mkdir");
    git(wt, &["mv", "builtin-log.c", "builtin/log.c"]);
    git(wt, &["commit", "-qm", "rename-to-builtin"]);

    let grit = grit_follow_commits(wt, "builtin/log.c", None);
    let git_oids = git_follow_commits(wt, "builtin/log.c", None);
    assert_eq!(
        grit, git_oids,
        "follow must list side edits merged before the mainline rename"
    );
    assert_eq!(grit.len(), 3, "rename, side-on-old-name, add");
}

#[test]
fn follow_merge_skips_side_line_legacy_path_edits() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    std::fs::create_dir_all(wt.join("tracked")).expect("mkdir");
    commit_file(wt, "tracked/file.txt", "main1\n", "m1");
    commit_file(wt, "tracked-file.txt", "legacy\n", "legacy");
    git(wt, &["branch", "side"]);
    commit_file(wt, "tracked-file.txt", "legacy\nside\n", "side-edit");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "tracked/file.txt", "main1\nmain2\n", "main-edit");
    git(wt, &["merge", "side", "-m", "merge"]);
    commit_file(
        wt,
        "tracked-file.txt",
        "legacy\nside\nafter\n",
        "post-merge-legacy-only",
    );

    let grit = grit_follow_commits(wt, "tracked/file.txt", None);
    let git_oids = git_follow_commits(wt, "tracked/file.txt", None);
    assert_eq!(
        grit, git_oids,
        "follow must not pick up parallel legacy-path side history"
    );
    assert_eq!(grit.len(), 2, "expected m1 and main-edit only");
}

#[test]
fn follow_skips_low_similarity_rename() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    commit_file(wt, "old.txt", "aaaaaaaaaa\n", "add");
    commit_file(wt, "other.txt", "bbbbbbbbbb\n", "noise");
    std::fs::remove_file(wt.join("old.txt")).expect("rm");
    std::fs::write(wt.join("new.txt"), "cccccccccc\n").expect("write new");
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-m", "unrelated-rename"]);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 1, "dissimilar rename must not extend history");
}

/// Compare the full OID list against system Git on a real repository (optional).
///
/// Set `GRIT_TEST_GIT_SRC` to a checkout of https://github.com/git/git with enough history
/// for `builtin/commit.c` (same as `git log --follow --format=%H -- builtin/commit.c`).
#[test]
fn follow_git_commit_c_history_matches_git_when_git_src_set() {
    let strict =
        std::env::var_os("GRIT_TEST_GIT_SRC_STRICT").is_some_and(|v| !v.is_empty() && v != "0");
    let Some(git_src) = std::env::var_os("GRIT_TEST_GIT_SRC") else {
        if strict {
            panic!("GRIT_TEST_GIT_SRC must be set when GRIT_TEST_GIT_SRC_STRICT=1");
        }
        return;
    };
    let wt = Path::new(&git_src);
    if !wt.join(".git").is_dir() {
        if strict {
            panic!(
                "GRIT_TEST_GIT_SRC must point at a git checkout when GRIT_TEST_GIT_SRC_STRICT=1"
            );
        }
        return;
    }

    let grit = grit_follow_commits(wt, "builtin/commit.c", None);
    let git_oids = git_follow_commits(wt, "builtin/commit.c", None);
    let gs: std::collections::HashSet<_> = grit.iter().collect();
    let extra: Vec<_> = grit
        .iter()
        .filter(|o| !git_oids.contains(o))
        .cloned()
        .collect();
    let missing: Vec<_> = git_oids
        .iter()
        .filter(|o| !gs.contains(o))
        .cloned()
        .collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "follow OID set mismatch: extra {extra:?} missing {missing:?} ({} vs {} commits)",
        grit.len(),
        git_oids.len()
    );
    assert_eq!(
        grit,
        git_oids,
        "follow history for builtin/commit.c must match git ({} vs {} commits)",
        grit.len(),
        git_oids.len()
    );
}

/// Issue #2459: `log --follow -- builtin-log.c` at dd2e794a21 on git/git (optional checkout).
#[test]
fn follow_git_dd2_log_c_history_matches_git_when_git_src_set() {
    let strict =
        std::env::var_os("GRIT_TEST_GIT_SRC_STRICT").is_some_and(|v| !v.is_empty() && v != "0");
    let Some(git_src) = std::env::var_os("GRIT_TEST_GIT_SRC") else {
        if strict {
            panic!("GRIT_TEST_GIT_SRC must be set when GRIT_TEST_GIT_SRC_STRICT=1");
        }
        return;
    };
    let wt = Path::new(&git_src);
    if !wt.join(".git").is_dir() {
        if strict {
            panic!(
                "GRIT_TEST_GIT_SRC must point at a git checkout when GRIT_TEST_GIT_SRC_STRICT=1"
            );
        }
        return;
    }

    let rev = "dd2e794a21";
    let repo = open_grit_repo(wt);
    let grit_at: Vec<String> = repo
        .rev_walk()
        .commits(
            &[rev],
            &RevWalkListOptions {
                paths: vec!["builtin-log.c".to_owned()],
                follow: true,
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit follow at dd2")
        .into_iter()
        .map(|oid| oid.to_hex())
        .collect();
    let git_at_rev: Vec<String> = git(
        wt,
        &["log", "--follow", "--pretty=%H", rev, "--", "builtin-log.c"],
    )
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty())
    .map(str::to_owned)
    .collect();
    let gs: std::collections::HashSet<_> = grit_at.iter().collect();
    let extra: Vec<_> = grit_at
        .iter()
        .filter(|o| !git_at_rev.contains(o))
        .cloned()
        .collect();
    let missing: Vec<_> = git_at_rev
        .iter()
        .filter(|o| !gs.contains(o))
        .cloned()
        .collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "follow at dd2e794a21 mismatch: extra {extra:?} missing {missing:?} ({} vs {} commits)",
        grit_at.len(),
        git_at_rev.len()
    );
    assert_eq!(
        grit_at,
        git_at_rev,
        "follow at dd2 must match git order ({} commits)",
        git_at_rev.len()
    );
}

/// Issue #2459: `log --follow -- builtin-log.c` at `4f62c2bc577b` on git/git (strict checkout).
#[test]
fn follow_git_log_c_at_4f62_builtin_log_c_history_matches_git_when_git_src_set() {
    let strict =
        std::env::var_os("GRIT_TEST_GIT_SRC_STRICT").is_some_and(|v| !v.is_empty() && v != "0");
    let Some(git_src) = std::env::var_os("GRIT_TEST_GIT_SRC") else {
        if strict {
            panic!("GRIT_TEST_GIT_SRC must be set when GRIT_TEST_GIT_SRC_STRICT=1");
        }
        return;
    };
    let wt = Path::new(&git_src);
    if !wt.join(".git").is_dir() {
        if strict {
            panic!(
                "GRIT_TEST_GIT_SRC must point at a git checkout when GRIT_TEST_GIT_SRC_STRICT=1"
            );
        }
        return;
    }

    let rev = "4f62c2bc577bbb85b65f3261c7fab7ef74def4cd";
    let repo = open_grit_repo(wt);
    let grit_at: Vec<String> = repo
        .rev_walk()
        .commits(
            &[rev],
            &RevWalkListOptions {
                paths: vec!["builtin-log.c".to_owned()],
                follow: true,
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit follow at 4f62")
        .into_iter()
        .map(|oid| oid.to_hex())
        .collect();
    let git_at_rev: Vec<String> = git(
        wt,
        &["log", "--follow", "--pretty=%H", rev, "--", "builtin-log.c"],
    )
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty())
    .map(str::to_owned)
    .collect();
    assert_eq!(
        grit_at.len(),
        git_at_rev.len(),
        "follow at 4f62 must match git commit count ({} vs {} commits)",
        grit_at.len(),
        git_at_rev.len()
    );
    assert_eq!(
        grit_at, git_at_rev,
        "follow at 4f62c2bc577b for builtin-log.c must match git order (179 commits)"
    );
    assert!(
        grit_at.first().is_some_and(|h| h.starts_with("4f62c2bc")),
        "tip commit 4f62c2bc must be listed when used as the start revision"
    );
    assert!(
        grit_at.iter().any(|h| h.starts_with("635530a2")),
        "635530a2 side-branch commit must remain in follow history at this tip"
    );
}

/// Issue #2459: `log --follow -- builtin/log.c` on git/git (optional full checkout).
#[test]
fn follow_git_log_c_history_matches_git_when_git_src_set() {
    let strict =
        std::env::var_os("GRIT_TEST_GIT_SRC_STRICT").is_some_and(|v| !v.is_empty() && v != "0");
    let Some(git_src) = std::env::var_os("GRIT_TEST_GIT_SRC") else {
        if strict {
            panic!("GRIT_TEST_GIT_SRC must be set when GRIT_TEST_GIT_SRC_STRICT=1");
        }
        return;
    };
    let wt = Path::new(&git_src);
    if !wt.join(".git").is_dir() {
        if strict {
            panic!(
                "GRIT_TEST_GIT_SRC must point at a git checkout when GRIT_TEST_GIT_SRC_STRICT=1"
            );
        }
        return;
    }

    if !strict {
        return;
    }

    let grit = grit_follow_commits(wt, "builtin/log.c", None);
    let git_oids = git_follow_commits(wt, "builtin/log.c", None);
    let gs: std::collections::HashSet<_> = grit.iter().collect();
    let extra: Vec<_> = grit
        .iter()
        .filter(|o| !git_oids.contains(o))
        .cloned()
        .collect();
    let missing: Vec<_> = git_oids
        .iter()
        .filter(|o| !gs.contains(o))
        .cloned()
        .collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "follow OID set mismatch for builtin/log.c: extra {extra:?} missing {missing:?} ({} vs {} commits)",
        grit.len(),
        git_oids.len()
    );
    assert_eq!(
        grit, git_oids,
        "follow history for builtin/log.c must match git order (611 commits at HEAD)"
    );
}
