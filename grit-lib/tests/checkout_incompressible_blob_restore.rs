//! Restoring a large incompressible blob must stay within a reasonable factor of system Git
//! wall time while keeping streaming memory use (see issue #2379).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::repo::Repository;

/// Incompressible payload size: large enough to catch whole-buffer inflate regressions.
const BLOB_BYTES: usize = 96 * 1024 * 1024;

/// Grit may be slower than Git on some hosts, but not several times slower on this path.
const MAX_GRIT_TO_GIT_RATIO: f64 = 2.75;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write_incompressible_blob(path: &Path) {
    let mut file = fs::File::create(path).expect("create");
    let mut buf = [0u8; 256 * 1024];
    let mut written = 0usize;
    let mut seed = 0x9E37_79B9_u32;
    while written < BLOB_BYTES {
        for b in &mut buf {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (seed >> 16) as u8;
        }
        let take = (BLOB_BYTES - written).min(buf.len());
        use std::io::Write;
        file.write_all(&buf[..take]).expect("write");
        written += take;
    }
}

fn restore_with_grit(root: &Path) {
    fs::remove_file(root.join("big.bin")).expect("rm");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["big.bin".into()],
            pathspec_sources: vec!["big.bin".into()],
            source: RestoreSource::Index,
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit restore");
}

fn restore_with_git(root: &Path) {
    fs::remove_file(root.join("big.bin")).expect("rm");
    git(root, &["restore", "big.bin"]);
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn time_restore(warmup: impl Fn(), run: impl Fn()) -> Duration {
    warmup();
    let mut samples = Vec::with_capacity(3);
    for _ in 0..3 {
        run();
        let start = Instant::now();
        run();
        samples.push(start.elapsed());
    }
    median(samples)
}

#[cfg_attr(debug_assertions, ignore = "wall-time gate: run with cargo test --release")]
#[test]
fn loose_incompressible_restore_is_not_orders_of_magnitude_slower_than_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    write_incompressible_blob(&root.join("big.bin"));
    git(root, &["add", "big.bin"]);
    git(root, &["commit", "-qm", "big"]);

    let git_time = time_restore(
        || restore_with_git(root),
        || restore_with_git(root),
    );
    let grit_time = time_restore(
        || restore_with_grit(root),
        || restore_with_grit(root),
    );

    let ratio = grit_time.as_secs_f64() / git_time.as_secs_f64().max(1e-9);
    assert!(
        ratio <= MAX_GRIT_TO_GIT_RATIO,
        "grit restore {grit_time:?} vs git {git_time:?} (ratio {ratio:.2}) exceeds {MAX_GRIT_TO_GIT_RATIO}"
    );

    git(root, &["fsck", "--no-progress"]);
    assert_eq!(fs::metadata(root.join("big.bin")).expect("meta").len(), BLOB_BYTES as u64);
}

#[cfg_attr(debug_assertions, ignore = "wall-time gate: run with cargo test --release")]
#[test]
fn packed_incompressible_restore_is_not_orders_of_magnitude_slower_than_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    write_incompressible_blob(&root.join("big.bin"));
    git(root, &["add", "big.bin"]);
    git(root, &["commit", "-qm", "big"]);
    git(root, &["repack", "-adq"]);

    let git_time = time_restore(
        || restore_with_git(root),
        || restore_with_git(root),
    );
    let grit_time = time_restore(
        || restore_with_grit(root),
        || restore_with_grit(root),
    );

    let ratio = grit_time.as_secs_f64() / git_time.as_secs_f64().max(1e-9);
    assert!(
        ratio <= MAX_GRIT_TO_GIT_RATIO,
        "packed: grit restore {grit_time:?} vs git {git_time:?} (ratio {ratio:.2}) exceeds {MAX_GRIT_TO_GIT_RATIO}"
    );
}
