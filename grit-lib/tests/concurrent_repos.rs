//! Two repositories on two threads with isolated [`Environment`] and cache state.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use grit_lib::command_runner::RecordingRunner;
use grit_lib::diagnostics::CollectingDiagnostics;
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::notes::{write_notes_commit, NotesTreeEntry};
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::transfer::{fetch_local, FetchOptions, TagMode};
use tempfile::TempDir;

const ITERS: usize = 8;

struct RepoFixture {
    root: PathBuf,
    env: Environment,
    author: String,
    diagnostics: Arc<CollectingDiagnostics>,
    command_runner: Arc<RecordingRunner>,
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
        diagnostics: Arc::new(CollectingDiagnostics::new()),
        command_runner: RecordingRunner::always_success(),
    }
}

fn open_with_env(fx: &RepoFixture) -> Repository {
    let git_dir = fx.root.join(".git");
    let opts = RepositoryOptions::with_environment(fx.env.clone())
        .with_command_runner(fx.command_runner.clone());
    let mut opts = opts;
    opts.diagnostics = fx.diagnostics.clone();
    Repository::open_with(&opts, &git_dir, Some(&fx.root)).expect("open")
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
        diagnostics: Arc::clone(&a.diagnostics),
        command_runner: Arc::clone(&a.command_runner),
    };
    let b2 = RepoFixture {
        root: b.root.clone(),
        env: b.env.clone(),
        author: b.author.clone(),
        diagnostics: Arc::clone(&b.diagnostics),
        command_runner: Arc::clone(&b.command_runner),
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

#[test]
fn two_repos_fetch_merge_notes_no_crosstalk() {
    let base = TempDir::new().expect("tempdir");
    let upstream = base.path().join("upstream.git");
    git_in(
        base.path(),
        &["init", "--bare", upstream.to_str().expect("path")],
    );
    let work = TempDir::new().expect("work clone");
    git_in(
        work.path(),
        &["clone", upstream.to_str().expect("path"), "."],
    );
    git_in(work.path(), &["config", "user.email", "u@example.com"]);
    git_in(work.path(), &["config", "user.name", "Upstream"]);
    std::fs::write(work.path().join("base.txt"), "base\n").expect("write");
    git_in(work.path(), &["add", "base.txt"]);
    git_in(work.path(), &["commit", "-m", "base"]);
    git_in(work.path(), &["branch", "topic"]);
    std::fs::write(work.path().join("topic.txt"), "topic\n").expect("write");
    git_in(work.path(), &["add", "topic.txt"]);
    git_in(work.path(), &["commit", "-m", "topic"]);
    let default_branch = git_in(work.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    git_in(work.path(), &["checkout", &default_branch]);
    std::fs::write(work.path().join("main.txt"), "main\n").expect("write");
    git_in(work.path(), &["add", "main.txt"]);
    git_in(work.path(), &["commit", "-m", "main"]);
    git_in(work.path(), &["push", "origin", &default_branch, "topic"]);

    let prepare = |name: &str, rerere: &str, email: &str| -> RepoFixture {
        let root = base.path().join(name);
        fs::create_dir_all(&root).expect("root");
        init_repository(&root, false, "main", None, "files").expect("init");
        let home = base.path().join(format!("home-{name}"));
        fs::create_dir_all(&home).expect("home");
        let global = home.join(".gitconfig");
        fs::write(
            &global,
            format!("[user]\n\tname = {name}\n\temail = {email}\n[merge]\n\trerere = {rerere}\n"),
        )
        .expect("global");
        fs::write(
            root.join(".git/config"),
            format!(
                "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
                upstream.display()
            ),
        )
        .expect("local config");
        std::fs::write(root.join("seed.txt"), "seed\n").expect("seed");
        let mut env = Environment::empty();
        env.cwd = root.clone();
        env.home = Some(home.into());
        env.git_config_global = Some(global.to_string_lossy().into_owned());
        env.git_config_nosystem = Some("true".into());
        env.git_config_system = Some("/dev/null".into());
        let diagnostics = Arc::new(CollectingDiagnostics::new());
        let command_runner = RecordingRunner::always_success();
        let fx = RepoFixture {
            root: root.clone(),
            env: env.clone(),
            author: name.to_owned(),
            diagnostics: Arc::clone(&diagnostics),
            command_runner: Arc::clone(&command_runner),
        };
        let repo = open_with_env(&fx);
        status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
        let ident = format!("{name} <{email}>");
        create_commit(
            &repo,
            &CommitRequest {
                message: "seed".into(),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
            },
            &mut NullProgress,
        )
        .expect("seed commit");
        RepoFixture {
            root,
            env,
            author: name.to_owned(),
            diagnostics,
            command_runner,
        }
    };

    let a = prepare("fetch-a", "true", "a@example.com");
    let b = prepare("fetch-b", "false", "b@example.com");
    fn run_fetch_merge_notes(
        fx: RepoFixture,
        upstream: &Path,
        default_branch: &str,
        ack: mpsc::Sender<(String, String, usize, usize)>,
    ) {
        let repo = open_with_env(&fx);
        let cfg = repo.config().expect("config");
        let rerere = cfg.get("merge.rerere").unwrap_or_default();
        let email = cfg.get("user.email").unwrap_or_default();
        fetch_local(
            &repo.git_dir,
            upstream,
            &FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
                tags: TagMode::None,
                ..Default::default()
            },
        )
        .expect("fetch");
        let main_ref = format!("refs/remotes/origin/{default_branch}");
        let main_tip = grit_lib::refs::resolve_ref(&repo.git_dir, &main_ref).expect("main tip");
        let topic_tip = grit_lib::refs::resolve_ref(&repo.git_dir, "refs/remotes/origin/topic")
            .expect("topic tip");
        let head = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD").expect("head");
        let _head_tree = parse_commit(&repo.odb.read(&head).expect("head obj").data)
            .expect("parse head")
            .tree;
        let main_tree = parse_commit(&repo.odb.read(&main_tip).expect("main obj").data)
            .expect("parse main")
            .tree;
        let topic_tree = parse_commit(&repo.odb.read(&topic_tip).expect("topic obj").data)
            .expect("parse topic")
            .tree;
        let bases = grit_lib::merge_base::merge_bases_first_vs_rest(&repo, main_tip, &[topic_tip])
            .expect("merge base");
        let merge_base = *bases.first().expect("non-empty merge base");
        let base_tree = parse_commit(&repo.odb.read(&merge_base).expect("base obj").data)
            .expect("parse base")
            .tree;
        let _merged = merge_trees_three_way(
            &repo,
            base_tree,
            main_tree,
            topic_tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
        )
        .expect("merge");
        write_notes_commit(
            &repo,
            "refs/notes/commits",
            &[NotesTreeEntry {
                mode: 0o100644,
                path: main_tip.to_hex().into_bytes(),
                oid: main_tip,
            }],
            &format!("note for {email}"),
        )
        .expect("notes");
        let hook_spawns = fx.command_runner.specs().len();
        let warnings = fx.diagnostics.warnings().len();
        ack.send((rerere, email, hook_spawns, warnings))
            .expect("ack");
    }

    let (tx_a, rx_a) = mpsc::channel();
    let (tx_b, rx_b) = mpsc::channel();
    let upstream_a = upstream.clone();
    let upstream_b = upstream.clone();
    let branch_a = default_branch.clone();
    let branch_b = default_branch.clone();
    let ha = thread::spawn({
        let fx = RepoFixture {
            root: a.root.clone(),
            env: a.env.clone(),
            author: a.author.clone(),
            diagnostics: Arc::clone(&a.diagnostics),
            command_runner: Arc::clone(&a.command_runner),
        };
        move || run_fetch_merge_notes(fx, &upstream_a, &branch_a, tx_a)
    });
    let hb = thread::spawn({
        let fx = RepoFixture {
            root: b.root.clone(),
            env: b.env.clone(),
            author: b.author.clone(),
            diagnostics: Arc::clone(&b.diagnostics),
            command_runner: Arc::clone(&b.command_runner),
        };
        move || run_fetch_merge_notes(fx, &upstream_b, &branch_b, tx_b)
    });

    let (rerere_a, email_a, hooks_a, warn_a) = rx_a
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("a");
    let (rerere_b, email_b, hooks_b, warn_b) = rx_b
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("b");
    ha.join().expect("join a");
    hb.join().expect("join b");

    assert_eq!(rerere_a, "true");
    assert_eq!(email_a, "a@example.com");
    assert_eq!(rerere_b, "false");
    assert_eq!(email_b, "b@example.com");
    assert_eq!(a.diagnostics.warnings().len(), warn_a);
    assert_eq!(b.diagnostics.warnings().len(), warn_b);
    assert_eq!(a.command_runner.specs().len(), hooks_a);
    assert_eq!(b.command_runner.specs().len(), hooks_b);
}
