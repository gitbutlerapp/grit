//! FSMN index extension compatibility with the system `git` binary and fsmonitor hook v2.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn isolated_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn write_fsmonitor_hook(dir: &Path) -> PathBuf {
    let hook = dir.join("fsmonitor-hook.sh");
    let script = r#"#!/bin/sh
if [ "$1" = "2" ]; then
  while read -r line; do
    [ -z "$line" ] && break
  done
  printf '%s\0' 'bench-token'
  exit 0
fi
exit 0
"#;
    fs::write(&hook, script).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).expect("chmod hook");
    }
    hook
}

fn fsck_clean(repo: &Path) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn ls_files_fsmonitor_flags(repo: &Path) -> Vec<(char, String)> {
    let out = isolated_git(repo, &["ls-files", "-f"]);
    out.lines()
        .filter_map(|line| {
            let (flag, path) = line.split_once(' ')?;
            flag.chars().next().map(|c| (c, path.to_string()))
        })
        .collect()
}

fn fsmn_token(repo: &Path) -> String {
    Index::load(&repo.join(".git/index"))
        .expect("load index")
        .fsmonitor_last_update
        .expect("FSMN token")
}

fn init_repo_with_fsmn(tmp: &Path) -> PathBuf {
    git(tmp, &["init", "-q", "-b", "main", "."]);
    git(tmp, &["config", "user.email", "t@example.com"]);
    git(tmp, &["config", "user.name", "Test"]);
    let hook = write_fsmonitor_hook(tmp);
    git(
        tmp,
        &[
            "config",
            "core.fsmonitor",
            hook.to_str().expect("hook utf8"),
        ],
    );
    for i in 0..40 {
        fs::write(tmp.join(format!("file-{i:02}.txt")), format!("v1 {i}\n")).expect("write");
    }
    git(tmp, &["add", "."]);
    git(tmp, &["commit", "-qm", "initial"]);
    isolated_git(tmp, &["status", "-s"]);
    let index = fs::read(tmp.join(".git/index")).expect("read index");
    assert!(
        index.windows(4).any(|w| w == b"FSMN"),
        "git must write FSMN after fsmonitor status"
    );
    tmp.to_path_buf()
}

fn make_replacement_entry(path: &[u8], content: &[u8]) -> IndexEntry {
    let oid = HashAlgo::Sha1.hash_object(ObjectKind::Blob, content);
    IndexEntry {
        ctime_sec: 1,
        ctime_nsec: 0,
        mtime_sec: 1,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: content.len() as u32,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    }
}

#[test]
fn grit_fsmn_index_round_trips_with_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = init_repo_with_fsmn(tmp.path());
    let token_before = fsmn_token(&repo_root);

    let grit_repo =
        Repository::open(&repo_root.join(".git"), Some(&repo_root)).expect("open grit repo");
    let mut index = grit_repo.load_index().expect("load index");
    assert_eq!(
        index.fsmonitor_last_update.as_deref(),
        Some(token_before.as_str())
    );

    for i in 0..5 {
        let path = format!("new-{i}.txt");
        fs::write(repo_root.join(&path), format!("added {i}\n")).expect("write worktree");
        let data = fs::read(repo_root.join(&path)).expect("read");
        let oid = grit_repo.odb.write(ObjectKind::Blob, &data).expect("blob");
        let mut entry = make_replacement_entry(path.as_bytes(), &data);
        entry.oid = oid;
        entry.set_fsmonitor_valid(true);
        index.add_or_replace(entry);
    }
    index.sort();
    grit_repo.write_index(&mut index).expect("write index");

    let tracked = isolated_git(&repo_root, &["ls-files"]).lines().count();
    let flags = ls_files_fsmonitor_flags(&repo_root);
    assert_eq!(flags.len(), tracked);
    assert!(
        flags.iter().all(|(c, _)| *c == 'h'),
        "all tracked paths must be fsmonitor-valid: {flags:?}"
    );

    let porcelain = isolated_git(&repo_root, &["status", "--porcelain"]);
    let mut porcelain_lines: Vec<_> = porcelain.lines().filter(|l| !l.is_empty()).collect();
    porcelain_lines.sort();
    let expected: Vec<String> = (0..5).map(|i| format!("A  new-{i}.txt")).collect();
    assert_eq!(porcelain_lines, expected, "unexpected porcelain output");
    fsck_clean(&repo_root);

    let token_after = fsmn_token(&repo_root);
    assert_eq!(token_after, token_before);
}

#[test]
fn git_fsmn_index_rewritten_by_grit_preserves_token() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = init_repo_with_fsmn(tmp.path());
    let token_before = fsmn_token(&repo_root);

    let grit_repo =
        Repository::open(&repo_root.join(".git"), Some(&repo_root)).expect("open grit repo");
    let mut index = grit_repo.load_index().expect("load");
    grit_repo.write_index(&mut index).expect("rewrite index");

    let token_after = fsmn_token(&repo_root);
    assert_eq!(token_after, token_before);

    let flags_before = ls_files_fsmonitor_flags(&repo_root);
    fsck_clean(&repo_root);
    assert_eq!(
        isolated_git(&repo_root, &["status", "--porcelain"]).trim(),
        ""
    );
    assert!(!flags_before.is_empty());
}
