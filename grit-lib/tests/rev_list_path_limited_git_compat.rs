//! Path-limited `rev-list` count and order vs system `git`.

mod support;

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::objects::ObjectId;
use grit_lib::revwalk::RevWalkListOptions;

use support::{git, git_rev_list, open_grit_repo, HistoryRepo};

fn commit_tree_with_file(
    wt: &std::path::Path,
    path: &str,
    content: &str,
    author_epoch: i64,
    parents: &[ObjectId],
) -> ObjectId {
    let full = wt.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(&full, content).expect("write");
    git(wt, &["add", path]);
    let tree = git(wt, &["write-tree"]).trim().to_string();
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "msg"];
    let parent_hex: Vec<String> = parents.iter().map(ObjectId::to_hex).collect();
    for hex in &parent_hex {
        args.push("-p");
        args.push(hex.as_str());
    }
    let out = Command::new("git")
        .current_dir(wt)
        .args(&args)
        .env("GIT_AUTHOR_DATE", format!("@{author_epoch}"))
        .env("GIT_COMMITTER_DATE", format!("@{author_epoch}"))
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("commit-tree");
    assert!(
        out.status.success(),
        "commit-tree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("oid")
}

fn grit_path_commits(
    wt: &std::path::Path,
    tokens: &[&str],
    path: &str,
    limit: Option<usize>,
) -> Vec<ObjectId> {
    let repo = open_grit_repo(wt);
    repo.rev_walk()
        .commits(
            tokens,
            &RevWalkListOptions {
                paths: vec![path.to_owned()],
                limit,
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit commits")
}

fn import_linear_touch_repo(wt: &std::path::Path, commits: usize) {
    git(wt, &["init", "-q"]);
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("fast-import");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        let mut t = 1_600_000_000i64;
        for i in 1..=commits {
            t += 100;
            let f = format!("d{}/f{}.txt", i % 5, i % 41);
            let data = format!("c{i}\n");
            let msg = format!("c{i}");
            writeln!(stdin, "commit refs/heads/main").unwrap();
            writeln!(stdin, "mark :{i}").unwrap();
            writeln!(stdin, "committer T <t@e> {t} +0000").unwrap();
            writeln!(stdin, "data {}", msg.len()).unwrap();
            write!(stdin, "{msg}").unwrap();
            if i > 1 {
                writeln!(stdin, "from :{}", i - 1).unwrap();
            }
            writeln!(stdin, "M 100644 inline {f}").unwrap();
            writeln!(stdin, "data {}", data.len()).unwrap();
            write!(stdin, "{data}").unwrap();
        }
    }
    let status = child.wait().expect("wait");
    assert!(status.success(), "fast-import failed");
    git(wt, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(wt, &["reset", "-q", "--hard"]);
}

#[test]
fn path_limited_count_and_limit_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 800);
    let repo = open_grit_repo(wt);

    let path = "d1/f3.txt";
    let git_count = git(wt, &["rev-list", "--count", "HEAD", "--", path])
        .trim()
        .parse::<usize>()
        .expect("git count");
    let grit_count = repo
        .rev_walk()
        .count(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![path.to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit count");
    assert_eq!(grit_count, git_count);

    let listed = repo
        .rev_walk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![path.to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit list");
    assert_eq!(listed.len(), grit_count);

    let git_limit = git(wt, &["rev-list", "-3", "HEAD", "--", path]);
    let grit_limit = repo
        .rev_walk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![path.to_owned()],
                limit: Some(3),
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit limit");
    let git_lines: Vec<_> = git_limit.lines().collect();
    assert_eq!(grit_limit.len(), git_lines.len());
    for (g, r) in git_lines.iter().zip(grit_limit.iter()) {
        assert_eq!(g, &r.to_hex());
    }
}

#[test]
fn path_limited_limit_picks_newer_tip_across_unrelated_tips() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["commit", "--allow-empty", "-m", "seed"]);
    let old = commit_tree_with_file(wt, "tracked.txt", "old\n", 1_577_836_800, &[]);
    let new = commit_tree_with_file(wt, "tracked.txt", "new\n", 1_577_923_200, &[]);
    let old_hex = old.to_hex();
    let new_hex = new.to_hex();

    let git_one = git_rev_list(
        wt,
        &[
            "-1",
            old_hex.as_str(),
            new_hex.as_str(),
            "--",
            "tracked.txt",
        ],
    );
    let grit_one = grit_path_commits(
        wt,
        &[old_hex.as_str(), new_hex.as_str()],
        "tracked.txt",
        Some(1),
    );
    assert_eq!(
        grit_one.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_one.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        "limit 1 across two tips must follow git date order (newer tip first)"
    );
}

#[test]
fn path_limited_limit_merge_prefers_newer_second_parent_side() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["commit", "--allow-empty", "-m", "seed"]);
    let base = commit_tree_with_file(wt, "tracked.txt", "base\n", 1_600_000_000, &[]);
    let main = commit_tree_with_file(
        wt,
        "tracked.txt",
        "main\n",
        1_600_001_000,
        std::slice::from_ref(&base),
    );
    let side = commit_tree_with_file(
        wt,
        "tracked.txt",
        "side\n",
        1_600_002_000,
        std::slice::from_ref(&base),
    );
    let merge_tree = git(wt, &["write-tree"]).trim().to_string();
    let main_hex = main.to_hex();
    let side_hex = side.to_hex();
    let args = [
        "commit-tree",
        merge_tree.as_str(),
        "-m",
        "merge",
        "-p",
        main_hex.as_str(),
        "-p",
        side_hex.as_str(),
    ];
    let out = Command::new("git")
        .current_dir(wt)
        .args(&args)
        .env("GIT_AUTHOR_DATE", "@1600003000")
        .env("GIT_COMMITTER_DATE", "@1600003000")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("merge commit-tree");
    assert!(out.status.success());
    let merge: ObjectId = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("merge oid");
    let merge_hex = merge.to_hex();

    let git_two = git_rev_list(wt, &["-2", merge_hex.as_str(), "--", "tracked.txt"]);
    let grit_two = grit_path_commits(wt, &[merge_hex.as_str()], "tracked.txt", Some(2));
    assert_eq!(
        grit_two.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_two.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        "limit 2 on merge must include newer side parent before main line"
    );
}

#[test]
fn path_limited_exclude_only_pathspec_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    std::fs::create_dir_all(wt.join("kept")).expect("mkdir");
    std::fs::write(wt.join("kept/file"), "x\n").expect("write");
    git(wt, &["add", "kept/file"]);
    git(wt, &["commit", "-m", "nested"]);
    let spec = ":(exclude)other/**";
    let git_count = git(wt, &["rev-list", "--count", "HEAD", "--", spec])
        .trim()
        .parse::<usize>()
        .expect("git count");
    let repo = open_grit_repo(wt);
    let grit_count = repo
        .rev_walk()
        .count(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![spec.into()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit count");
    assert_eq!(grit_count, git_count);
    assert_eq!(git_count, 1);
}

#[test]
fn path_limited_root_gitlink_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["commit", "--allow-empty", "-m", "target"]);
    let existing = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    let tree = format!("160000 commit {existing}\tsub\n");
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["mktree"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("mktree");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(tree.as_bytes())
        .expect("write tree");
    let out = child.wait_with_output().expect("mktree wait");
    assert!(out.status.success());
    let tree_oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let orphan = git(wt, &["commit-tree", tree_oid.as_str(), "-m", "orphan"])
        .trim()
        .to_string();
    let git_count = git(wt, &["rev-list", "--count", orphan.as_str(), "--", "sub"])
        .trim()
        .parse::<usize>()
        .expect("git count");
    let repo = open_grit_repo(wt);
    let grit_count = repo
        .rev_walk()
        .count(
            &[orphan.as_str()],
            &RevWalkListOptions {
                paths: vec!["sub".into()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit count");
    assert_eq!(grit_count, git_count);
    assert_eq!(git_count, 1);
}

fn grit_path_count(wt: &std::path::Path, path: &str) -> usize {
    open_grit_repo(wt)
        .rev_walk()
        .count(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![path.to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit count")
}

fn git_path_count(wt: &std::path::Path, path: &str) -> usize {
    git(wt, &["rev-list", "--count", "HEAD", "--", path])
        .trim()
        .parse()
        .expect("git count")
}

#[test]
fn path_limited_single_directory_name_count_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    std::fs::create_dir_all(wt.join("foo")).expect("mkdir");
    std::fs::write(wt.join("foo/a.txt"), "1\n").expect("write");
    git(wt, &["add", "foo/a.txt"]);
    git(wt, &["commit", "-m", "add foo"]);
    std::fs::write(wt.join("foo/a.txt"), "2\n").expect("write");
    git(wt, &["add", "foo/a.txt"]);
    git(wt, &["commit", "-m", "touch foo"]);

    let git_count = git_path_count(wt, "foo");
    let grit_count = grit_path_count(wt, "foo");
    assert_eq!(grit_count, git_count);
    assert_eq!(git_count, 2);
}

#[test]
fn path_limited_directory_trailing_slash_count_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 40);
    for path in ["d1", "d1/"] {
        let git_count = git_path_count(wt, path);
        let grit_count = grit_path_count(wt, path);
        assert_eq!(grit_count, git_count, "path {path:?}");
        assert!(git_count > 0, "path {path:?}");
    }
}

#[test]
fn path_limited_nested_directory_count_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    std::fs::create_dir_all(wt.join("outer/inner")).expect("mkdir");
    std::fs::write(wt.join("outer/inner/file.txt"), "1\n").expect("write");
    git(wt, &["add", "outer/inner/file.txt"]);
    git(wt, &["commit", "-m", "add nested"]);
    std::fs::write(wt.join("outer/inner/file.txt"), "2\n").expect("write");
    git(wt, &["add", "outer/inner/file.txt"]);
    git(wt, &["commit", "-m", "touch nested"]);

    for path in ["outer/inner", "outer/inner/"] {
        let git_count = git_path_count(wt, path);
        let grit_count = grit_path_count(wt, path);
        assert_eq!(grit_count, git_count, "path {path:?}");
        assert_eq!(git_count, 2, "path {path:?}");
    }
}

#[test]
fn path_limited_glob_under_directory_count_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 40);
    let path = "d1/*.txt";
    let git_count = git_path_count(wt, path);
    let grit_count = grit_path_count(wt, path);
    assert_eq!(grit_count, git_count);
    assert!(git_count > 0);
}

#[test]
fn path_limited_dot_pathspec_matches_unrestricted_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 40);
    let unrestricted: usize = git(wt, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .expect("git unrestricted count");
    for path in [".", "./"] {
        assert_eq!(git_path_count(wt, path), unrestricted, "git path {path:?}");
        assert_eq!(
            grit_path_count(wt, path),
            unrestricted,
            "grit path {path:?}"
        );
    }
}

#[test]
fn path_limited_dot_slash_file_and_dir_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    std::fs::create_dir_all(wt.join("outer/inner")).expect("mkdir");
    std::fs::write(wt.join("outer/inner/file.txt"), "1\n").expect("write");
    git(wt, &["add", "outer/inner/file.txt"]);
    git(wt, &["commit", "-m", "one"]);
    std::fs::write(wt.join("outer/inner/file.txt"), "2\n").expect("write");
    git(wt, &["add", "outer/inner/file.txt"]);
    git(wt, &["commit", "-m", "two"]);

    for path in ["./outer/inner/file.txt", "./outer/inner/"] {
        let git_count = git_path_count(wt, path);
        let grit_count = grit_path_count(wt, path);
        assert_eq!(grit_count, git_count, "path {path:?}");
        assert_eq!(git_count, 2, "path {path:?}");
    }
}

#[test]
fn path_limited_glob_star_not_literal_prefix_overmatch() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 40);
    let glob = "d1/f1*";
    let git_glob = git_path_count(wt, glob);
    let grit_glob = grit_path_count(wt, glob);
    assert_eq!(grit_glob, git_glob);
    let git_one = git_path_count(wt, "d1/f1.txt");
    let grit_one = grit_path_count(wt, "d1/f1.txt");
    assert_eq!(grit_one, git_one);
    assert_ne!(
        git_glob, git_one,
        "fixture should distinguish glob from single file"
    );
}
