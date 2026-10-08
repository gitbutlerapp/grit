//! Two repositories on two threads with isolated [`Environment`] and cache state.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;

use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{init_repository, Repository};
use tempfile::TempDir;

const ITERS: usize = 8;

struct RepoFixture {
    root: PathBuf,
    env: Environment,
    author: String,
}

fn prepare_repo(
    base: &Path,
    name: &str,
    author: &str,
    autocrlf: &str,
    attr_line: &str,
) -> RepoFixture {
    let root = base.join(name);
    fs::create_dir_all(&root).expect("repo root");
    init_repository(&root, false, "main", None, "files").expect("init");

    let home = base.join(format!("home-{name}"));
    fs::create_dir_all(&home).expect("home");
    let global = home.join(".gitconfig");
    fs::write(
        &global,
        format!(
            "[user]\n\tname = {author}\n\temail = {author}@example.com\n[core]\n\tautocrlf = {autocrlf}\n"
        ),
    )
    .expect("global config");
    fs::write(root.join(".gitattributes"), format!("{attr_line}\n")).expect("gitattributes");

    let mut env = Environment::empty();
    env.cwd = root.clone();
    env.home = Some(home.into());
    env.git_config_global = Some(global.to_string_lossy().into_owned());
    env.git_config_nosystem = Some("true".into());
    env.git_config_system = Some("/dev/null".into());

    RepoFixture {
        root,
        env,
        author: author.to_owned(),
    }
}

fn open_with_env(fx: &RepoFixture) -> Repository {
    let git_dir = fx.root.join(".git");
    Repository::open_with(
        &RepositoryOptions::with_environment(fx.env.clone()),
        &git_dir,
        Some(&fx.root),
    )
    .expect("open")
}

fn run_repo_loop(fx: RepoFixture, ack: mpsc::Sender<()>) {
    for i in 0..ITERS {
        let repo = open_with_env(&fx);
        let cfg = repo.config().expect("config");
        assert_eq!(
            cfg.get("user.name").as_deref(),
            Some(fx.author.as_str()),
            "iteration {i}: wrong user.name"
        );
        let autocrlf = cfg.get("core.autocrlf").unwrap_or_default();
        let expect_autocrlf = if fx.author == "Alice" {
            "true"
        } else {
            "false"
        };
        assert_eq!(
            autocrlf.to_ascii_lowercase(),
            expect_autocrlf,
            "iteration {i}: wrong core.autocrlf"
        );

        let path = fx.root.join(format!("file-{i}.txt"));
        fs::write(&path, format!("content {i}\n")).expect("write");
        status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
        let ident = format!("{} <{}@example.com> 1 +0000", fx.author, fx.author);
        create_commit(
            &repo,
            &CommitRequest {
                message: format!("commit {i}"),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
            },
            &mut NullProgress,
        )
        .expect("commit");
    }
    ack.send(()).expect("ack");
}

fn git_in(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn two_repos_on_two_threads() {
    let base = TempDir::new().expect("tempdir");
    let a = prepare_repo(base.path(), "a", "Alice", "true", "* text=auto");
    let b = prepare_repo(base.path(), "b", "Bob", "false", "* -text");

    let (tx_a, rx_a) = mpsc::channel();
    let (tx_b, rx_b) = mpsc::channel();

    let a2 = RepoFixture {
        root: a.root.clone(),
        env: a.env.clone(),
        author: a.author.clone(),
    };
    let b2 = RepoFixture {
        root: b.root.clone(),
        env: b.env.clone(),
        author: b.author.clone(),
    };

    let ha = thread::spawn(move || run_repo_loop(a2, tx_a));
    let hb = thread::spawn(move || run_repo_loop(b2, tx_b));

    rx_a.recv_timeout(std::time::Duration::from_secs(120))
        .expect("thread a");
    rx_b.recv_timeout(std::time::Duration::from_secs(120))
        .expect("thread b");
    ha.join().expect("join a");
    hb.join().expect("join b");

    for (repo, author, expect_text_attr) in [(&a.root, "Alice", "auto"), (&b.root, "Bob", "unset")]
    {
        git_in(repo, &["fsck"]);
        let log_authors = git_in(repo, &["log", "--format=%an"]);
        for line in log_authors.lines() {
            assert_eq!(line, author, "git log author in {}", repo.display());
        }
        let attr = git_in(repo, &["check-attr", "text", "--", "file-0.txt"]);
        assert!(
            attr.contains(expect_text_attr),
            "expected text={expect_text_attr} for {author}, got: {attr}"
        );
    }
}
