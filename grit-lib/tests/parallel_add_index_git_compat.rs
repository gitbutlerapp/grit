//! Parallel `stage` must write the same index as a single-threaded run (SHA-1 and SHA-256).

use std::fs;
use std::path::Path;
use std::process::Command;

use filetime::FileTime;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_grit_repo(root: &Path, sha256: bool) -> Repository {
    let git_dir = root.join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");
    let config = if sha256 {
        "[core]\n\trepositoryformatversion = 1\n\tbare = false\n\tfilemode = true\n[extensions]\n\tobjectformat = sha256\n[user]\n\temail = t@example.com\n\tname = Test\n"
    } else {
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n[user]\n\temail = t@example.com\n\tname = Test\n"
    };
    fs::write(git_dir.join("config"), config).expect("config");
    Repository::open(&git_dir, Some(root)).expect("open grit repo")
}

fn write_wide_tree(root: &Path, count: usize) {
    for i in 0..count {
        let dir = format!("d{:04}", i % 100);
        fs::create_dir_all(root.join(&dir)).expect("mkdir");
        let path = format!("{dir}/f{i:05}.txt");
        fs::write(root.join(&path), format!("payload {i}\n")).expect("write");
    }
}

fn pin_all_file_mtrees(root: &Path, count: usize) {
    let pinned = FileTime::from_unix_time(1_700_000_000, 0);
    for i in 0..count {
        let dir = format!("d{:04}", i % 100);
        let path = root.join(format!("{dir}/f{i:05}.txt"));
        filetime::set_file_mtime(&path, pinned).expect("pin mtime");
    }
}

fn reset_staging_area(repo: &Repository) {
    let git_dir = repo.git_dir.as_path();
    let _ = fs::remove_file(git_dir.join("index"));
    let objects = git_dir.join("objects");
    if objects.is_dir() {
        for ent in fs::read_dir(&objects).expect("read objects") {
            let ent = ent.expect("dirent");
            let path = ent.path();
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn set_index_threads(repo: &Repository, threads: usize) {
    let path = repo.git_dir.join("config");
    let mut text = fs::read_to_string(&path).expect("config");
    text.push_str(&format!("\n[index]\n\tthreads = {threads}\n"));
    fs::write(&path, text).expect("write config");
}

fn stage_all(repo: &Repository, threads: Option<usize>) -> Vec<u8> {
    if let Some(n) = threads {
        set_index_threads(repo, n);
    }
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    fs::read(repo.index_path()).expect("read index")
}

fn git_fsck_ok(repo: &Path) -> bool {
    Command::new("git")
        .args(["-C", repo.to_str().expect("utf-8 path"), "fsck"])
        .status()
        .expect("spawn git fsck")
        .success()
}

fn assert_serial_parallel_index_match(sha256: bool) {
    const N: usize = 10_000;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_wide_tree(root, N);
    pin_all_file_mtrees(root, N);
    let repo = init_grit_repo(root, sha256);

    let serial_index = stage_all(&repo, Some(1));
    reset_staging_area(&repo);
    let parallel_threads = std::thread::available_parallelism()
        .map(|n| n.get().max(2))
        .unwrap_or(2);
    let parallel_index = stage_all(&repo, Some(parallel_threads));

    if serial_index != parallel_index {
        let diff_at = serial_index
            .iter()
            .zip(parallel_index.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(serial_index.len().min(parallel_index.len()));
        panic!(
            "parallel and serial stage must produce identical index bytes (len {} vs {}, first diff at {})",
            serial_index.len(),
            parallel_index.len(),
            diff_at
        );
    }

    assert!(git_fsck_ok(root), "git fsck after grit index");
    assert!(
        git(root, &["diff-files"]).trim().is_empty(),
        "git diff-files must be clean after grit add"
    );
    git(root, &["commit", "-qm", "initial"]);
    assert!(
        git(root, &["status", "--porcelain"]).trim().is_empty(),
        "git status --porcelain must be clean after commit"
    );
}

#[test]
fn parallel_add_index_matches_serial_sha1() {
    assert_serial_parallel_index_match(false);
}

#[test]
fn parallel_add_index_matches_serial_sha256() {
    assert_serial_parallel_index_match(true);
}
