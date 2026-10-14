//! Path-limited `rev-list` count and order vs system `git`.

mod support;

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::RevWalkListOptions;

use support::{git, HistoryRepo};

fn open_grit(wt: &std::path::Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
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
    let repo = open_grit(wt);

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
