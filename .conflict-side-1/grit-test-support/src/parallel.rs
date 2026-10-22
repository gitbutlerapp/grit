//! Helpers for forcing parallel code paths in library integration tests.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

#[cfg(unix)]
const HEAVY_DISK_READY: &str = "grit-heavy-disk-ready\n";

#[cfg(unix)]
struct ProcessHeavyDiskLock {
    holder: std::process::Child,
}

#[cfg(unix)]
impl ProcessHeavyDiskLock {
    fn acquire() -> std::io::Result<Self> {
        let path = heavy_disk_lock_path();
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        let path_s = path
            .to_str()
            .ok_or_else(|| std::io::Error::other("heavy-disk lock path is not UTF-8"))?;
        let mut holder = Command::new("flock")
            .args([
                "--exclusive",
                path_s,
                "-c",
                "echo grit-heavy-disk-ready; exec sleep 86400",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdout = holder
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("flock stdout pipe missing"))?;
        let mut stderr = holder
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("flock stderr pipe missing"))?;

        read_exact_or_child_failure(
            &mut stdout,
            &mut holder,
            &mut stderr,
            HEAVY_DISK_READY.as_bytes(),
        )?;

        Ok(Self { holder })
    }
}

#[cfg(unix)]
fn read_exact_or_child_failure(
    stdout: &mut impl Read,
    child: &mut std::process::Child,
    stderr: &mut impl Read,
    expected: &[u8],
) -> std::io::Result<()> {
    let mut buf = vec![0_u8; expected.len()];
    let mut filled = 0_usize;
    while filled < expected.len() {
        if let Some(status) = child.try_wait()? {
            let mut err_body = String::new();
            let _ = stderr.read_to_string(&mut err_body);
            return Err(std::io::Error::other(format!(
                "flock helper exited with {status} before lock ready (stderr: {err_body})"
            )));
        }
        match stdout.read(&mut buf[filled..]) {
            Ok(0) => {
                if let Some(status) = child.try_wait()? {
                    let mut err_body = String::new();
                    let _ = stderr.read_to_string(&mut err_body);
                    return Err(std::io::Error::other(format!(
                        "flock helper closed stdout with {status} (stderr: {err_body})"
                    )));
                }
                return Err(std::io::Error::other(
                    "flock helper closed stdout before lock ready marker",
                ));
            }
            Ok(n) => filled += n,
            Err(e) => return Err(e),
        }
    }
    if buf != expected {
        return Err(std::io::Error::other(format!(
            "unexpected flock ready marker: {:?}",
            String::from_utf8_lossy(&buf)
        )));
    }
    Ok(())
}

#[cfg(unix)]
impl Drop for ProcessHeavyDiskLock {
    fn drop(&mut self) {
        let pid = self.holder.id();
        if pid != 0 {
            let _ = Command::new("pkill")
                .args(["-P", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.holder.kill();
        let _ = self.holder.wait();
    }
}

#[cfg(not(unix))]
mod in_process_fallback {
    use std::sync::{Condvar, Mutex, MutexGuard};

    /// In-process waiters when advisory flock is unavailable (non-Unix targets).
    #[derive(Default)]
    pub(super) struct HeavyDiskSerialGate {
        pub(super) active: bool,
    }

    pub(super) struct InProcessHeavyDiskGuard {
        pub(super) gate: MutexGuard<'static, HeavyDiskSerialGate>,
    }

    impl Drop for InProcessHeavyDiskGuard {
        fn drop(&mut self) {
            self.gate.active = false;
            in_process_condvar().notify_one();
        }
    }

    fn in_process_gate() -> &'static Mutex<HeavyDiskSerialGate> {
        static GATE: Mutex<HeavyDiskSerialGate> = Mutex::new(HeavyDiskSerialGate { active: false });
        &GATE
    }

    fn in_process_condvar() -> &'static Condvar {
        static CV: Condvar = Condvar::new();
        &CV
    }

    pub(super) fn acquire_in_process() -> InProcessHeavyDiskGuard {
        let mut gate = in_process_gate()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while gate.active {
            gate = in_process_condvar()
                .wait(gate)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        gate.active = true;
        InProcessHeavyDiskGuard { gate }
    }
}

/// Serialize tests that materialize tens of thousands of worktree files.
///
/// On Unix, `flock(1)` on a shared lockfile coordinates across parallel `cargo test`
/// binaries (CI disk peaks). Other platforms fall back to in-process serialization via
/// a process-wide mutex gate.
pub struct HeavyDiskSerialGuard {
    #[cfg(unix)]
    _process: ProcessHeavyDiskLock,
    #[cfg(not(unix))]
    _in_process: in_process_fallback::InProcessHeavyDiskGuard,
}

/// Acquire the heavy-disk fixture lock; held until the returned guard is dropped.
pub fn heavy_disk_serial_guard() -> HeavyDiskSerialGuard {
    #[cfg(unix)]
    {
        let _process = ProcessHeavyDiskLock::acquire().unwrap_or_else(|e| {
            panic!("grit heavy-disk test flock: {e}");
        });
        HeavyDiskSerialGuard { _process }
    }
    #[cfg(not(unix))]
    {
        HeavyDiskSerialGuard {
            _in_process: in_process_fallback::acquire_in_process(),
        }
    }
}

#[cfg(test)]
mod heavy_disk_lock_tests {
    #[cfg(unix)]
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

    #[cfg(unix)]
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

        let blocked = Command::new("flock")
            .args(["-n", "--exclusive", path_s, "-c", "exit 0"])
            .status()
            .expect("spawn non-blocking flock probe");
        assert!(
            blocked.code() == Some(1),
            "expected flock -n to fail while lock held, got {blocked}"
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
