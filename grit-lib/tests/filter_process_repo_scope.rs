//! Repository-scoped filter-process registry: one long-running driver per repo handle.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::diff::hash_worktree_file;
use grit_lib::repo::Repository;
use grit_lib::worktree_rules::WorktreeRules;

fn have_python3() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_ok_and(|s| s.success())
}

fn fixture_filter_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/filter_process_test_server.py")
}

fn init_repo_with_process_filter(wt: &Path, state_file: &Path) -> PathBuf {
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(wt)
        .status()
        .expect("git init");
    let git_dir = wt.join(".git");
    let script = fixture_filter_script();
    let process_cmd = format!("python3 -u {}", script.display());
    Command::new("git")
        .args(["config", "filter.test.process", &process_cmd])
        .current_dir(wt)
        .status()
        .expect("git config process");
    fs::write(wt.join(".gitattributes"), "* filter=test\n").expect("attributes");
    fs::write(state_file, "").expect("truncate state");
    std::env::set_var("GRIT_FILTER_STATE_FILE", state_file);
    git_dir
}

fn count_state_lines(state_file: &Path) -> usize {
    fs::read_to_string(state_file)
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

#[test]
fn filter_process_reused_and_disable_persists_on_repository() {
    if !have_python3() {
        return;
    }
    let td = tempfile::tempdir().expect("tempdir");
    let wt = td.path();
    let state_file = wt.join("filter_pids.txt");
    let git_dir = init_repo_with_process_filter(wt, &state_file);

    fs::write(wt.join("a.txt"), b"hello\n").expect("a");
    fs::write(wt.join("b.txt"), b"world\n").expect("b");

    let repo = Repository::open(&git_dir, Some(wt)).expect("open");
    let index = repo.load_index().expect("index");
    let rules = WorktreeRules::from_repository(&repo, &index).expect("rules");
    let filter_fp = Some(rules.filter_process());
    let conv = rules.conversion().clone();

    let meta_a = fs::symlink_metadata(wt.join("a.txt")).expect("meta a");
    let attrs_a = rules.file_attrs("a.txt", false);
    assert!(
        attrs_a.filter_process.is_some(),
        "expected filter.test.process on file attributes"
    );
    let _ = hash_worktree_file(
        &repo.odb,
        &wt.join("a.txt"),
        &meta_a,
        &conv,
        &attrs_a,
        "a.txt",
        None,
        filter_fp,
    )
    .expect("hash a");

    let meta_b = fs::symlink_metadata(wt.join("b.txt")).expect("meta b");
    let attrs_b = rules.file_attrs("b.txt", false);
    let oid_b = hash_worktree_file(
        &repo.odb,
        &wt.join("b.txt"),
        &meta_b,
        &conv,
        &attrs_b,
        "b.txt",
        None,
        filter_fp,
    )
    .expect("hash b");

    assert_eq!(
        count_state_lines(&state_file),
        1,
        "expected one filter process for two paths on the same repository handle"
    );

    let upper = b"WORLD\n";
    let upper_oid = repo.odb.hash(grit_lib::objects::ObjectKind::Blob, upper);
    assert_eq!(
        oid_b, upper_oid,
        "filter clean should uppercase worktree bytes"
    );

    let proc_cmd = attrs_b
        .filter_process
        .as_deref()
        .expect("process filter configured");
    rules.filter_process().disable_process_filter(proc_cmd);

    let oid_b_after = hash_worktree_file(
        &repo.odb,
        &wt.join("b.txt"),
        &meta_b,
        &conv,
        &attrs_b,
        "b.txt",
        None,
        filter_fp,
    )
    .expect("hash b after disable");
    let raw_oid = repo
        .odb
        .hash(grit_lib::objects::ObjectKind::Blob, b"world\n");
    assert_eq!(
        oid_b_after, raw_oid,
        "disabled filter should leave worktree bytes unmodified for hashing"
    );
    assert_eq!(
        count_state_lines(&state_file),
        1,
        "disable should not spawn a replacement filter process"
    );
}
