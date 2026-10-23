//! Unit tests for list limits on notes, bundles, and hooks.

use grit_lib::notes::NotesRef;
use grit_lib::repo::{InitOptions, Repository};
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "T"]);
}

fn seed_commit(dir: &Path, file: &str, body: &str, msg: &str) -> grit_lib::objects::ObjectId {
    fs::write(dir.join(file), body).expect("write");
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"]).parse().expect("oid")
}

#[test]
fn notes_list_limit_stops_early() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    for i in 0..5 {
        let oid = seed_commit(dir.path(), "f.txt", &format!("{i}\n"), &format!("c{i}"));
        git(
            dir.path(),
            &["notes", "add", "-m", &format!("n{i}"), &oid.to_hex()],
        );
    }
    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open");
    let limited = repo
        .notes()
        .list(&NotesRef::commits(), Some(2))
        .expect("list");
    assert_eq!(limited.items.len(), 2);
    assert!(limited.truncated);

    let git_list = git(dir.path(), &["notes", "list"]);
    let git_lines: Vec<_> = git_list.lines().take(2).collect();
    let grit_lines: Vec<String> = limited
        .items
        .iter()
        .map(|n| format!("{} {}", n.note_blob.to_hex(), n.object.to_hex()))
        .collect();
    assert_eq!(grit_lines, git_lines, "first N must match git notes list");
}

#[test]
fn bundle_list_heads_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    seed_commit(dir.path(), "a.txt", "1\n", "one");
    git(dir.path(), &["branch", "b1"]);
    seed_commit(dir.path(), "a.txt", "2\n", "two");
    git(dir.path(), &["branch", "b2"]);
    let bundle = dir.path().join("all.bundle");
    git(
        dir.path(),
        &["bundle", "create", bundle.to_str().unwrap(), "--all"],
    );
    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open");
    let listed = repo
        .bundles()
        .list_heads(&bundle, Some(2))
        .expect("list_heads");
    assert_eq!(listed.items.len(), 2);
    assert!(listed.truncated);
}

#[test]
fn hooks_list_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let hooks_dir = dir.path().join(".git").join("hooks");
    for name in ["aaa", "bbb", "ccc"] {
        fs::write(hooks_dir.join(name), "#!/bin/sh\n").expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(hooks_dir.join(name), fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
    }
    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open");
    let listed = repo.hooks().list(Some(2)).expect("list");
    assert_eq!(listed.items.len(), 2);
    assert!(listed.truncated);
    assert_eq!(listed.items[0].name, "aaa");
    assert_eq!(listed.items[1].name, "bbb");
}
