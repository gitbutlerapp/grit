//! Path-limited `rev_walk` log order vs system `git log --format=%H`.

use grit_lib::progress::{CancelToken, NullProgress};

mod support;

use grit_lib::revwalk::RevWalkListOptions;
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};

use support::{git, git_rev_list, open_grit_repo, HistoryRepo};

fn import_rename_repo(wt: &std::path::Path) {
    use std::fs;
    git(wt, &["init", "-q"]);
    fs::write(wt.join("old.txt"), b"1\n").expect("write old");
    git(wt, &["add", "old.txt"]);
    git(wt, &["commit", "-q", "-m", "add old"]);
    git(wt, &["mv", "old.txt", "new.txt"]);
    git(wt, &["commit", "-q", "-m", "rename"]);
    git(wt, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(wt, &["reset", "-q", "--hard"]);
}

fn import_linear_touch_repo(wt: &std::path::Path, commits: usize) {
    use std::io::Write;
    use std::process::{Command, Stdio};

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
    assert!(child.wait().expect("wait").success());
    git(wt, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(wt, &["reset", "-q", "--hard"]);
}

#[test]
fn log_pathspec_literal_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 120);
    let path = "d1/f3.txt";
    let git_oids = git_rev_list(wt, &["HEAD", "--", path]);
    let repo = open_grit_repo(wt);
    let grit_oids = repo
        .revwalk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                pathspec: Pathspec::scoped([path], PathspecGlobalFlags::default()),
                ..RevWalkListOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit commits");
    assert_eq!(grit_oids.iter().copied().collect::<Vec<_>>(), git_oids);
}

#[test]
fn log_pathspec_glob_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 80);
    let pattern = "d*/f*.txt";
    let git_oids = git_rev_list(wt, &["HEAD", "--", pattern]);
    let repo = open_grit_repo(wt);
    let grit_oids = repo
        .revwalk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                pathspec: Pathspec::scoped([pattern], PathspecGlobalFlags::default()),
                ..RevWalkListOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit commits");
    assert_eq!(grit_oids.iter().copied().collect::<Vec<_>>(), git_oids);
}

#[test]
fn log_pathspec_exclude_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 40);
    let spec = ":(exclude)d2/**";
    let git_oids = git_rev_list(wt, &["HEAD", "--", spec]);
    let repo = open_grit_repo(wt);
    let grit_oids = repo
        .revwalk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                pathspec: Pathspec::scoped([spec], PathspecGlobalFlags::default()),
                ..RevWalkListOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit commits");
    assert_eq!(grit_oids.iter().copied().collect::<Vec<_>>(), git_oids);
}

#[test]
fn log_pathspec_icase_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_linear_touch_repo(wt, 30);
    let spec = ":(icase)D1/F3.TXT";
    let git_oids = git_rev_list(wt, &["HEAD", "--", spec]);
    let repo = open_grit_repo(wt);
    let grit_oids = repo
        .revwalk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                pathspec: Pathspec::scoped([spec], PathspecGlobalFlags::default()),
                ..RevWalkListOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit commits");
    assert_eq!(grit_oids.iter().copied().collect::<Vec<_>>(), git_oids);
}

#[test]
fn log_pathspec_after_rename_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_rename_repo(wt);
    let path = "new.txt";
    let git_oids = git_rev_list(wt, &["HEAD", "--", path]);
    let repo = open_grit_repo(wt);
    let grit_oids = repo
        .revwalk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                pathspec: Pathspec::scoped([path], PathspecGlobalFlags::default()),
                ..RevWalkListOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit commits");
    assert_eq!(grit_oids.iter().copied().collect::<Vec<_>>(), git_oids);
}

#[test]
fn log_pathspec_rename_follow_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    import_rename_repo(wt);
    let path = "new.txt";
    let git_oids = git_rev_list(wt, &["--follow", "HEAD", "--", path]);
    let repo = open_grit_repo(wt);
    let grit_oids = repo
        .revwalk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                pathspec: Pathspec::scoped([path], PathspecGlobalFlags::default()),
                follow: true,
                ..RevWalkListOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit commits");
    assert_eq!(grit_oids.iter().copied().collect::<Vec<_>>(), git_oids);
}
