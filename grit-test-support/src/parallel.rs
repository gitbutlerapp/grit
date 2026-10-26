//! Helpers for forcing parallel code paths in library integration tests.

use std::fs::OpenOptions;
use std::path::PathBuf;

use grit_lib::parallel::Parallelism;
use grit_lib::parallel::{ParallelOp, ParallelOptions, ThreadPolicy};

/// Build a [`ThreadPolicy`] that forces four workers and disables batch thresholds.
#[must_use]
pub fn forced_parallel_policy() -> ThreadPolicy {
    ThreadPolicy::with_threads_override(Some(4))
        .min_items_override(Some(0))
        .min_total_bytes_override(Some(0))
}

/// Resolved options for `op` with [`forced_parallel_policy`].
#[must_use]
pub fn forced_parallel_options(op: ParallelOp) -> ParallelOptions {
    forced_parallel_policy().resolve(op, &grit_lib::config::ConfigSet::new())
}

/// [`Parallelism`] with four workers (clamped to available CPUs).
#[must_use]
pub fn forced_parallelism() -> Parallelism {
    forced_parallel_options(ParallelOp::IndexRefresh).parallelism()
}

thread_local! {
    static TEST_HEAVY_DISK_LOCK: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn heavy_disk_lock_path() -> PathBuf {
    if let Some(path) = TEST_HEAVY_DISK_LOCK.with(|slot| slot.borrow().clone()) {
        return path;
    }
    if let Ok(path) = std::env::var("GRIT_HEAVY_DISK_LOCK") {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join(".grit-heavy-disk-test.lock")
}

#[cfg(test)]
fn set_test_heavy_disk_lock(path: PathBuf) {
    TEST_HEAVY_DISK_LOCK.with(|slot| *slot.borrow_mut() = Some(path));
}

/// Serialize tests that materialize tens of thousands of worktree files.
///
/// Holds an exclusive lock on a shared lockfile (`$TMPDIR/.grit-heavy-disk-test.lock`, or
/// the path in `GRIT_HEAVY_DISK_LOCK`), so parallel test processes take turns (CI disk
/// peaks). It is the operating system's file lock, released when the guard drops or the
/// process exits.
pub struct HeavyDiskSerialGuard {
    _file: std::fs::File,
}

/// Acquire the heavy-disk fixture lock; held until the returned guard is dropped.
///
/// # Panics
///
/// When the lockfile cannot be opened or locked.
pub fn heavy_disk_serial_guard() -> HeavyDiskSerialGuard {
    let path = heavy_disk_lock_path();
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .and_then(|file| file.lock().map(|()| file))
        .unwrap_or_else(|e| panic!("grit heavy-disk test lock {}: {e}", path.display()));
    HeavyDiskSerialGuard { _file: file }
}

#[cfg(test)]
mod heavy_disk_lock_tests {
    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    #[ignore = "spawned by heavy_disk_serial_guard_excludes_concurrent_process"]
    fn heavy_disk_serial_guard_probe_hold() {
        let _g = super::heavy_disk_serial_guard();
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        writeln!(out, "grit-heavy-disk-probe-held").expect("probe stdout");
        out.flush().expect("probe stdout flush");
        std::thread::sleep(std::time::Duration::from_secs(600));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn heavy_disk_serial_guard_excludes_concurrent_process() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let _lock_dir = tempfile::tempdir().expect("isolated lock tempdir");
        let lock_path = _lock_dir.path().join("heavy-disk.lock");
        let path_s = lock_path.to_str().expect("lock path utf-8");
        super::set_test_heavy_disk_lock(lock_path.clone());

        let _guard = super::heavy_disk_serial_guard();

        let other = std::fs::File::open(&lock_path).expect("open lockfile again");
        assert!(
            matches!(other.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "a second handle must not get the lock while the guard holds it"
        );

        let exe = std::env::current_exe().expect("current test executable");
        let mut child = Command::new(exe)
            .args([
                "parallel::heavy_disk_lock_tests::heavy_disk_serial_guard_probe_hold",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("GRIT_HEAVY_DISK_LOCK", path_s)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn probe subprocess");

        let mut reader = BufReader::new(child.stdout.take().expect("probe stdout"));
        let hold_window = Instant::now() + Duration::from_secs(2);
        while Instant::now() < hold_window {
            if child.try_wait().expect("probe try_wait").is_some() {
                panic!("probe subprocess exited before acquiring the contended lock");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        drop(_guard);

        let acquire_deadline = Instant::now() + Duration::from_secs(60);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).expect("read probe stdout") > 0
                && line.contains("grit-heavy-disk-probe-held")
            {
                break;
            }
            if Instant::now() >= acquire_deadline {
                panic!("probe subprocess did not acquire lock after holder dropped");
            }
            if let Some(status) = child.try_wait().expect("probe try_wait") {
                panic!("probe subprocess exited early with {status}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}
