//! Path-limited `rev-list` count and order vs system `git`.

mod support;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::RevWalkListOptions;

use support::{git, git_rev_list, open_grit_repo, HistoryRepo};

fn open_grit_repo_from_cwd(worktree: &Path, cwd: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = cwd.to_path_buf();
    let mut opts = RepositoryOptions::empty();
    opts.environment = env;
    Repository::discover(Some(worktree), &opts).expect("discover grit repo from cwd")
}

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
fn path_limited_from_subdirectory_cwd_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(wt.join("sub")).expect("mkdir sub");
    std::fs::create_dir_all(wt.join("other")).expect("mkdir other");
    std::fs::write(wt.join("sub/f.txt"), "1\n").expect("write sub");
    std::fs::write(wt.join("other/g.txt"), "1\n").expect("write other");
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-m", "one"]);
    std::fs::write(wt.join("other/g.txt"), "2\n").expect("touch other");
    git(wt, &["commit", "-am", "two"]);
    std::fs::write(wt.join("sub/f.txt"), "2\n").expect("touch sub");
    git(wt, &["commit", "-am", "three"]);

    let sub = wt.join("sub");
    let git_count_file = Command::new("git")
        .current_dir(&sub)
        .args(["rev-list", "--count", "HEAD", "--", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-list")
        .stdout;
    let git_count_file: usize = String::from_utf8_lossy(&git_count_file)
        .trim()
        .parse()
        .expect("git count f.txt");
    assert_eq!(git_count_file, 2);

    let git_count_dot = Command::new("git")
        .current_dir(&sub)
        .args(["rev-list", "--count", "HEAD", "--", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-list dot")
        .stdout;
    let git_count_dot: usize = String::from_utf8_lossy(&git_count_dot)
        .trim()
        .parse()
        .expect("git count dot");
    assert_eq!(git_count_dot, 2);

    let repo = open_grit_repo_from_cwd(wt, &sub);
    let grit_count_file = repo
        .rev_walk()
        .count(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec!["f.txt".to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit count f.txt");
    assert_eq!(grit_count_file, git_count_file);

    let grit_count_dot = repo
        .rev_walk()
        .count(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![".".to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit count dot");
    assert_eq!(grit_count_dot, git_count_dot);

    let git_commits = Command::new("git")
        .current_dir(&sub)
        .args(["rev-list", "HEAD", "--", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-list list")
        .stdout;
    let git_hex: Vec<String> = String::from_utf8_lossy(&git_commits)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();

    let grit_commits = repo
        .rev_walk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec!["f.txt".to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit list");
    assert_eq!(
        grit_commits
            .iter()
            .map(ObjectId::to_hex)
            .collect::<Vec<_>>(),
        git_hex
    );

    let top_spec = ":(top)sub/f.txt".to_owned();
    let git_top_count = Command::new("git")
        .current_dir(&sub)
        .args(["rev-list", "--count", "HEAD", "--", top_spec.as_str()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-list top")
        .stdout;
    let git_top_count: usize = String::from_utf8_lossy(&git_top_count)
        .trim()
        .parse()
        .expect("git top count");
    assert_eq!(git_top_count, git_count_file);

    let grit_top_count = repo
        .rev_walk()
        .count(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![top_spec],
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit top count");
    assert_eq!(grit_top_count, git_top_count);
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

/// Fast-import a history with a merge, a wide root tree, and `flip` turning from a file into a
/// directory and back, then repack it so root trees are stored as deep delta chains.
fn import_packed_flip_repo(wt: &std::path::Path) {
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
        let mut blob = |stdin: &mut std::process::ChildStdin, mode: &str, path: &str, i: usize| {
            let data = format!("{path} {i}\n");
            writeln!(stdin, "M {mode} inline {path}").unwrap();
            writeln!(stdin, "data {}", data.len()).unwrap();
            write!(stdin, "{data}").unwrap();
        };
        let mut header = |stdin: &mut std::process::ChildStdin, branch: &str, mark: usize| {
            t += 100;
            writeln!(stdin, "commit refs/heads/{branch}").unwrap();
            writeln!(stdin, "mark :{mark}").unwrap();
            writeln!(stdin, "committer T <t@e> {t} +0000").unwrap();
            writeln!(stdin, "data 1\nm").unwrap();
        };
        for i in 1..=120usize {
            header(stdin, "main", i);
            if i > 1 {
                writeln!(stdin, "from :{}", i - 1).unwrap();
            }
            if i == 1 {
                for w in 0..200 {
                    blob(stdin, "100644", &format!("wide{w:03}.txt"), 0);
                }
            }
            if i == 100 || i == 110 {
                writeln!(stdin, "merge :1060").unwrap();
            }
            if i == 110 {
                // Take the side branch's README.md, so this merge is TREESAME to its second
                // parent only.
                blob(stdin, "100644", "README.md", 1006);
            }
            match i {
                40 | 80 => writeln!(stdin, "D flip").unwrap(),
                _ => {}
            }
            if i % 7 == 0 || i == 1 {
                blob(stdin, "100644", "README.md", i);
            }
            if i % 5 == 0 || i == 1 {
                blob(stdin, "100644", "docs/a.txt", i);
            }
            if (40..80).contains(&i) {
                if i % 3 == 0 || i == 40 {
                    blob(stdin, "100644", "flip/x", i);
                }
            } else if i % 3 == 0 || i == 1 || i == 80 {
                blob(stdin, "100755", "flip", i);
            }
            blob(stdin, "100644", &format!("wide{:03}.txt", i % 200), i);
            if i == 60 {
                for s in 1..=6usize {
                    header(stdin, "side", 1060 + s - 6);
                    let from = if s == 1 { 60 } else { 1060 + s - 7 };
                    writeln!(stdin, "from :{from}").unwrap();
                    if s % 2 == 0 {
                        blob(stdin, "100644", "README.md", 1000 + s);
                    }
                    blob(stdin, "100644", "docs/side.txt", s);
                }
            }
        }
    }
    let status = child.wait().expect("wait");
    assert!(status.success(), "fast-import failed");
    git(wt, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(wt, &["repack", "-adfq", "--depth=50", "--window=250"]);
    git(wt, &["prune-packed"]);
}

#[test]
fn path_limited_packed_delta_trees_match_git_log() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_packed_flip_repo(wt);
    assert!(
        std::fs::read_dir(wt.join(".git/objects/pack"))
            .expect("pack dir")
            .any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "pack"))),
        "fixture must be packed"
    );
    for path in [
        "README.md",
        "docs/a.txt",
        "docs/side.txt",
        "docs",
        "flip",
        "flip/x",
        "wide042.txt",
        "missing",
        "missing/deeper",
    ] {
        let expected: Vec<String> = git(wt, &["rev-list", "HEAD", "--", path])
            .lines()
            .map(str::to_owned)
            .collect();
        let got: Vec<String> = grit_path_commits(wt, &["HEAD"], path, None)
            .iter()
            .map(ObjectId::to_hex)
            .collect();
        assert_eq!(got, expected, "path {path}");
    }
    let flip = git(wt, &["rev-list", "--count", "HEAD", "--", "flip"]);
    assert!(flip.trim().parse::<usize>().expect("count") > 10);
}
