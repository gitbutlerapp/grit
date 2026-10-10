//! Two repositories on two threads with isolated [`Environment`] and cache state.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use grit_lib::command_runner::{RecordingRunner, ShellInvocation};
use grit_lib::diagnostics::{CollectingDiagnostics, Warning};
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::hooks::{run_hook_opts, RunHookOptions};
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::merge_diff::blob_oid_at_path;
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::notes::{write_notes_commit, NotesTreeEntry};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::rerere::{repo_rerere, RerereAutoupdate, RerereEventKind};
use grit_lib::transfer::{fetch_local, FetchOptions, TagMode};
use tempfile::TempDir;

const ITERS: usize = 8;
const CONCURRENT_ITERS: usize = if cfg!(debug_assertions) { 8 } else { 20 };

struct RepoFixture {
    root: PathBuf,
    env: Environment,
    author: String,
    diagnostics: Arc<CollectingDiagnostics>,
    command_runner: Arc<RecordingRunner>,
    reference_unix_time: i64,
    hook_marker: String,
    hook_event: &'static str,
    traditional_hook: &'static str,
    rerere_enabled: bool,
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
    init_repository(
        &root,
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");

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
        reference_unix_time: 0,
        hook_marker: String::new(),
        hook_event: "pre-commit",
        traditional_hook: "pre-commit",
        rerere_enabled: false,
    }
}

fn open_with_env(fx: &RepoFixture) -> Repository {
    let git_dir = fx.root.join(".git");
    let opts = RepositoryOptions::with_environment(fx.env.clone())
        .with_command_runner(fx.command_runner.clone());
    let mut opts = opts;
    opts.diagnostics = fx.diagnostics.clone();
    opts.reference_unix_time = Some(fx.reference_unix_time);
    Repository::open_with(&opts, &git_dir, Some(&fx.root)).expect("open")
}

fn runner_records_marker(runner: &RecordingRunner, marker: &str) -> bool {
    runner.specs().iter().any(|spec| {
        let shell_has = spec.shell.as_ref().is_some_and(|shell| match shell {
            ShellInvocation::DashC { script, .. } => script.contains(marker),
            ShellInvocation::ScriptPath { .. } => false,
        });
        shell_has
            || spec.program.to_string_lossy().contains(marker)
            || spec
                .args
                .iter()
                .any(|a| a.to_string_lossy().contains(marker))
    })
}

fn index_conflict_entry(path: &str, stage: u16, oid: ObjectId) -> IndexEntry {
    let path_b = path.as_bytes().to_vec();
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: (stage << 12) | (path_b.len().min(0x0fff) as u16),
        flags_extended: None,
        path: path_b,
        base_index_pos: 0,
    }
}

fn has_non_executable_hook_warning(diag: &CollectingDiagnostics, hook_name: &str) -> bool {
    diag.warnings().iter().any(|w| {
        matches!(
            w,
            Warning::NonExecutableHookIgnored {
                hook_name: name
            } if name == hook_name
        )
    })
}

#[cfg(unix)]
fn write_non_executable_hook(hook_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(hook_path, "#!/bin/sh\necho trad\n").expect("hook body");
    let mut perms = fs::metadata(hook_path).expect("hook meta").permissions();
    perms.set_mode(0o644);
    fs::set_permissions(hook_path, perms).expect("hook perms");
}

#[cfg(not(unix))]
fn write_non_executable_hook(hook_path: &Path) {
    fs::write(hook_path, "#!/bin/sh\necho trad\n").expect("hook body");
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
                sign_override: None,
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
        reference_unix_time: a.reference_unix_time,
        hook_marker: a.hook_marker.clone(),
        hook_event: a.hook_event,
        traditional_hook: a.traditional_hook,
        rerere_enabled: a.rerere_enabled,
    };
    let b2 = RepoFixture {
        root: b.root.clone(),
        env: b.env.clone(),
        author: b.author.clone(),
        diagnostics: Arc::clone(&b.diagnostics),
        command_runner: Arc::clone(&b.command_runner),
        reference_unix_time: b.reference_unix_time,
        hook_marker: b.hook_marker.clone(),
        hook_event: b.hook_event,
        traditional_hook: b.traditional_hook,
        rerere_enabled: b.rerere_enabled,
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
    for iter in 0..CONCURRENT_ITERS {
        run_fetch_merge_notes_no_crosstalk_once(iter);
    }
}

struct ThreadReport {
    email: String,
    wall_epoch: i64,
    rerere_preimage: bool,
    notes_commit_author: String,
}

fn run_fetch_merge_notes_no_crosstalk_once(iter: usize) {
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
    std::fs::write(work.path().join("shared.txt"), "base\n").expect("write shared");
    git_in(work.path(), &["add", "shared.txt"]);
    git_in(work.path(), &["commit", "-m", "base"]);
    git_in(work.path(), &["branch", "topic"]);
    std::fs::write(work.path().join("shared.txt"), "topic\n").expect("topic shared");
    git_in(work.path(), &["add", "shared.txt"]);
    git_in(work.path(), &["commit", "-m", "topic"]);
    let default_branch = git_in(work.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    git_in(work.path(), &["checkout", &default_branch]);
    std::fs::write(work.path().join("shared.txt"), "main\n").expect("main shared");
    git_in(work.path(), &["add", "shared.txt"]);
    git_in(work.path(), &["commit", "-m", "main"]);
    git_in(work.path(), &["push", "origin", &default_branch, "topic"]);

    let prepare = |name: &str,
                   email: &str,
                   reference_unix_time: i64,
                   hook_marker: &str,
                   hook_event: &'static str,
                   traditional_hook: &'static str,
                   rerere_enabled: bool|
     -> RepoFixture {
        let root = base.path().join(format!("{name}-{iter}"));
        fs::create_dir_all(&root).expect("root");
        init_repository(
            &root,
            false,
            "main",
            None,
            grit_lib::RefStorageFormat::Files,
        )
        .expect("init");
        let home = base.path().join(format!("home-{name}-{iter}"));
        fs::create_dir_all(&home).expect("home");
        let global = home.join(".gitconfig");
        fs::write(
            &global,
            format!("[user]\n\tname = {name}\n\temail = {email}\n"),
        )
        .expect("global");
        let mut local_cfg = format!(
            "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
            upstream.display()
        );
        if rerere_enabled {
            local_cfg.push_str("\n[rerere]\n\tenabled = true\n");
            fs::create_dir_all(root.join(".git/rr-cache")).expect("rr-cache");
        }
        local_cfg.push_str(&format!(
            "\n[hook \"hc\"]\n\tcommand = {hook_marker}\n\tevent = {hook_event}\n"
        ));
        fs::write(root.join(".git/config"), local_cfg).expect("local config");
        fs::create_dir_all(root.join(".git/hooks")).expect("hooks dir");
        write_non_executable_hook(&root.join(".git/hooks").join(traditional_hook));
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
            reference_unix_time,
            hook_marker: hook_marker.to_owned(),
            hook_event,
            traditional_hook,
            rerere_enabled,
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
                sign_override: None,
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
            reference_unix_time,
            hook_marker: hook_marker.to_owned(),
            hook_event,
            traditional_hook,
            rerere_enabled,
        }
    };

    let a = prepare(
        "fetch-a",
        "a@example.com",
        1_111_111_111,
        "hook-marker-a",
        "pre-commit",
        "pre-commit",
        true,
    );
    let b = prepare(
        "fetch-b",
        "b@example.com",
        2_222_222_222,
        "hook-marker-b",
        "commit-msg",
        "commit-msg",
        false,
    );

    fn run_fetch_merge_notes(
        fx: RepoFixture,
        upstream: &Path,
        default_branch: &str,
        ack: mpsc::Sender<ThreadReport>,
    ) {
        let repo = open_with_env(&fx);
        assert_eq!(
            repo.wall_clock_epoch(),
            fx.reference_unix_time,
            "wall clock must stay on the repository handle"
        );
        let cfg = repo.config().expect("config");
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
        let path = "shared.txt";
        let base = blob_oid_at_path(&repo.odb, &base_tree, path).expect("base blob");
        let ours = blob_oid_at_path(&repo.odb, &main_tree, path).expect("ours blob");
        let theirs = blob_oid_at_path(&repo.odb, &topic_tree, path).expect("theirs blob");
        let conflict_body = "<<<<<<< ours\nmain\n=======\ntopic\n>>>>>>> theirs\n";
        fs::write(fx.root.join(path), conflict_body).expect("wt conflict");
        let mut index = Index::new();
        index.add_or_replace(index_conflict_entry(path, 1, base));
        index.add_or_replace(index_conflict_entry(path, 2, ours));
        index.add_or_replace(index_conflict_entry(path, 3, theirs));
        repo.write_index(&mut index).expect("write conflict index");
        let rerere_events = repo_rerere(&repo, RerereAutoupdate::No).expect("rerere");
        let rerere_preimage = rerere_events
            .iter()
            .any(|e| e.path == "shared.txt" && e.kind == RerereEventKind::RecordedPreimage);
        run_hook_opts(
            Some(&repo),
            fx.hook_event,
            &[],
            cfg.as_ref(),
            RunHookOptions::default(),
            None,
        )
        .expect("configured hook");
        run_hook_opts(
            Some(&repo),
            fx.traditional_hook,
            &[],
            cfg.as_ref(),
            RunHookOptions::default(),
            None,
        )
        .expect("traditional hook scan");
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
        let notes_tip =
            grit_lib::refs::resolve_ref(&repo.git_dir, "refs/notes/commits").expect("notes tip");
        let notes_commit =
            parse_commit(&repo.odb.read(&notes_tip).expect("notes commit object").data)
                .expect("parse notes commit");
        ack.send(ThreadReport {
            email: email.clone(),
            wall_epoch: repo.wall_clock_epoch(),
            rerere_preimage,
            notes_commit_author: notes_commit.author,
        })
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
            reference_unix_time: a.reference_unix_time,
            hook_marker: a.hook_marker.clone(),
            hook_event: a.hook_event,
            traditional_hook: a.traditional_hook,
            rerere_enabled: a.rerere_enabled,
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
            reference_unix_time: b.reference_unix_time,
            hook_marker: b.hook_marker.clone(),
            hook_event: b.hook_event,
            traditional_hook: b.traditional_hook,
            rerere_enabled: b.rerere_enabled,
        };
        move || run_fetch_merge_notes(fx, &upstream_b, &branch_b, tx_b)
    });

    let report_a = rx_a
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("a");
    let report_b = rx_b
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("b");
    ha.join().expect("join a");
    hb.join().expect("join b");

    assert_eq!(report_a.email, "a@example.com");
    assert_eq!(report_b.email, "b@example.com");
    assert_eq!(report_a.wall_epoch, 1_111_111_111);
    assert_eq!(report_b.wall_epoch, 2_222_222_222);
    assert!(
        report_a.rerere_preimage,
        "repo A should record rerere preimage"
    );
    assert!(
        !report_b.rerere_preimage,
        "repo B should not record rerere preimage"
    );
    assert!(
        report_a.notes_commit_author.contains("a@example.com"),
        "notes commit on A must use repo A config identity, got {}",
        report_a.notes_commit_author
    );
    assert!(
        report_b.notes_commit_author.contains("b@example.com"),
        "notes commit on B must use repo B config identity, got {}",
        report_b.notes_commit_author
    );
    assert!(
        !report_a.notes_commit_author.contains("b@example.com"),
        "repo A notes must not pick up repo B identity"
    );
    assert!(
        !report_b.notes_commit_author.contains("a@example.com"),
        "repo B notes must not pick up repo A identity"
    );

    assert!(
        runner_records_marker(&a.command_runner, "hook-marker-a"),
        "repo A runner must record configured hook argv"
    );
    assert!(
        !runner_records_marker(&a.command_runner, "hook-marker-b"),
        "repo B hook marker must not appear in repo A runner"
    );
    assert!(
        runner_records_marker(&b.command_runner, "hook-marker-b"),
        "repo B runner must record configured hook argv"
    );
    assert!(
        !runner_records_marker(&b.command_runner, "hook-marker-a"),
        "repo A hook marker must not appear in repo B runner"
    );

    assert!(has_non_executable_hook_warning(
        &a.diagnostics,
        "pre-commit"
    ));
    assert!(!has_non_executable_hook_warning(
        &b.diagnostics,
        "pre-commit"
    ));
    assert!(has_non_executable_hook_warning(
        &b.diagnostics,
        "commit-msg"
    ));
    assert!(!has_non_executable_hook_warning(
        &a.diagnostics,
        "commit-msg"
    ));
}
