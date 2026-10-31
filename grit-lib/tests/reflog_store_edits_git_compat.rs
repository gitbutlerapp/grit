//! Reflog reads, deletes and expiry through
//! [`RepositoryReflogs`](grit_lib::repo_handles::RepositoryReflogs) on a files repository go
//! through its ref store and leave logs the system `git` reads the same way.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::{ExpireOptions, ReflogQuery};

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";

fn open_files(dir: &Path) -> Repository {
    Repository::init(dir, &InitOptions::new().with_initial_branch("main")).expect("init");
    let config = dir.join(".git/config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str("[user]\n\tname = T\n\temail = t@e.x\n");
    fs::write(&config, text).expect("write config");
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::with_environment(env).with_work_tree(dir),
    )
    .expect("open")
}

fn commit_n(repo: &Repository, work_tree: &Path, n: usize) {
    for i in 0..n {
        fs::write(work_tree.join("a"), format!("{i}\n")).expect("write");
        stage(
            repo,
            &StageOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("stage");
        commit_with_scope(
            repo,
            &{
                let mut o = CommitRequest::default();
                o.message = format!("c{i}");
                o.author = Some(IDENT_TS.parse().expect("signature"));
                o.committer = Some(IDENT_TS.parse().expect("signature"));
                o
            },
            CommitScope::Staged,
            &mut NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("commit");
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_reflog(dir: &Path, refname: &str) -> Vec<String> {
    git(dir, &["reflog", "show", "--format=%H", refname])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn files_repo_reflog_edits_stay_readable_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = open_files(tmp.path());
    commit_n(&repo, tmp.path(), 4);
    let head = RefName::try_from("HEAD").expect("HEAD");

    let page = repo
        .reflogs()
        .query(
            &head,
            &ReflogQuery {
                limit: Some(1),
                ..ReflogQuery::default()
            },
        )
        .expect("query");
    assert_eq!(page.entries.len(), 1);
    assert!(page.truncated);
    assert_eq!(
        vec![page.entries[0].entry.new_oid.to_string()],
        git_reflog(tmp.path(), "HEAD")[..1]
    );

    repo.reflogs().delete(&head, &[0], true).expect("delete");
    let grit: Vec<String> = repo
        .reflogs()
        .entries(&head)
        .expect("entries")
        .iter()
        .map(|e| e.entry.new_oid.to_string())
        .collect();
    assert_eq!(grit.len(), 3);
    assert_eq!(git_reflog(tmp.path(), "HEAD"), grit);

    let out = repo
        .reflogs()
        .expire(&ExpireOptions {
            now: 1_800_000_000,
            expire: Some("now".into()),
            expire_unreachable: Some("now".into()),
            all: false,
            dry_run: false,
        })
        .expect("expire");
    assert_eq!(out.entries_pruned, 7);
    assert!(
        tmp.path().join(".git/logs/HEAD").is_file(),
        "an emptied log is kept, as Git keeps it"
    );
    assert!(git_reflog(tmp.path(), "HEAD").is_empty());
    git(tmp.path(), &["fsck", "--no-progress"]);
}
