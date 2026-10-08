//! Manual timing for [`grit_lib::diff::refresh_index_stat_content_verified`] only.
//!
//! Run: `cargo test -p grit-lib --test index_refresh_only_bench -- --ignored --nocapture`

use std::time::Instant;

use grit_lib::diff::refresh_index_stat_content_verified;
use grit_lib::repo::Repository;
use grit_test_support::git;

#[test]
#[ignore = "manual refresh-only benchmark"]
fn refresh_index_stat_only_timing() {
    const N: usize = 5_000;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..N {
        std::fs::write(root.join(format!("f{i:05}.txt")), format!("body {i}\n")).expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c"]);

    for i in 0..N {
        let path = root.join(format!("f{i:05}.txt"));
        filetime::set_file_mtime(&path, filetime::FileTime::now()).expect("touch");
    }

    let repo = Repository::open(&root.join(".git"), Some(root)).expect("open");
    let mut index = repo.load_index().expect("index");
    let index_mtime = index.source_mtime;
    let config = grit_lib::config::ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )
    .unwrap_or_default();
    let start = Instant::now();
    let _ = refresh_index_stat_content_verified(
        &repo.odb,
        &repo.git_dir,
        &mut index,
        root,
        index_mtime,
        Some(&config),
        None,
    )
    .expect("refresh");
    let elapsed = start.elapsed();
    eprintln!("refresh_index_stat_content_verified only: {elapsed:?}");
    std::fs::write(
        "/opt/cursor/artifacts/refresh-only-timing.txt",
        format!("{elapsed:?}\n"),
    )
    .ok();
}
