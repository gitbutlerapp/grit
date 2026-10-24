//! Exclusive lock files with optional Git-style acquisition backoff.
//!
//! Grit uses lock files (`.lock` siblings, `index.lock`, `packed-refs.lock`, …) the same way
//! Git does: create exclusively, write staged content, then rename over the target path.
//! [`LockFile::acquire`] retries with jittered exponential backoff when configured, matching
//! `core.filesRefLockTimeout`, `core.packedRefsTimeout`, and `reftable.lockTimeout`.
//!
//! ```rust
//! use grit_lib::config::ConfigSet;
//! use grit_lib::lockfile::LockWait;
//! let wait = LockWait::files_ref_lock(Some(&ConfigSet::new()));
//! assert!(matches!(wait, LockWait::Immediately | LockWait::Backoff { .. } | LockWait::Indefinitely));
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::ConfigSet;
use crate::environment::Environment;

/// Whether to wait for an existing lock, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWait {
    /// Fail immediately when the lock file already exists.
    Immediately,
    /// Retry with jittered exponential backoff until `budget` elapses.
    Backoff {
        /// Total time budget for acquisition attempts.
        budget: Duration,
    },
    /// Retry until the lock is acquired (Git `core.filesRefLockTimeout = -1`).
    Indefinitely,
}

impl LockWait {
    /// `core.filesRefLockTimeout` (milliseconds), default 100 ms when unset.
    #[must_use]
    pub fn files_ref_lock(config: Option<&ConfigSet>) -> Self {
        Self::from_files_ref_lock_timeout_ms(Self::parse_files_ref_lock_timeout_ms(config))
    }

    /// Parse `core.filesRefLockTimeout` the way Git does (milliseconds).
    #[must_use]
    pub fn parse_files_ref_lock_timeout_ms(config: Option<&ConfigSet>) -> i64 {
        config
            .and_then(|cfg| cfg.get("core.filesreflocktimeout"))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(100)
    }

    #[must_use]
    fn from_files_ref_lock_timeout_ms(timeout_ms: i64) -> Self {
        match timeout_ms {
            0 => Self::Immediately,
            ms if ms < 0 => Self::Indefinitely,
            ms => Self::Backoff {
                budget: Duration::from_millis(ms as u64),
            },
        }
    }

    /// `core.packedRefsTimeout` (milliseconds), default 1000 ms when unset.
    #[must_use]
    pub fn packed_refs(config: Option<&ConfigSet>) -> Self {
        Self::from_milliseconds_config(config, "core.packedrefstimeout", 1_000)
    }

    /// `reftable.lockTimeout` (milliseconds), default 1000 ms when unset.
    #[must_use]
    pub fn reftable_tables_list(config: Option<&ConfigSet>) -> Self {
        Self::from_milliseconds_config(config, "reftable.locktimeout", 1_000)
    }

    /// Load repository config from `git_dir` and return [`Self::files_ref_lock`].
    #[must_use]
    pub fn files_ref_lock_for_git_dir(git_dir: &Path) -> Self {
        let config = ConfigSet::load(&Environment::empty(), Some(git_dir), true).ok();
        Self::files_ref_lock(config.as_ref())
    }

    /// Load repository config from `git_dir` and return [`Self::packed_refs`].
    #[must_use]
    pub fn packed_refs_for_git_dir(git_dir: &Path) -> Self {
        let config = ConfigSet::load(&Environment::empty(), Some(git_dir), true).ok();
        Self::packed_refs(config.as_ref())
    }

    fn from_milliseconds_config(config: Option<&ConfigSet>, key: &str, default_ms: u64) -> Self {
        let ms = config
            .and_then(|cfg| cfg.get(key))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map(|v| v.max(0) as u64)
            .unwrap_or(default_ms);
        if ms == 0 {
            Self::Immediately
        } else {
            Self::Backoff {
                budget: Duration::from_millis(ms),
            }
        }
    }
}

/// Failure to create or commit a lock file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LockError {
    /// Another process holds the lock (or the lock path already exists).
    #[error("lock held at {path}")]
    Held {
        /// Path to the lock file that could not be created exclusively.
        path: PathBuf,
    },

    /// Underlying I/O failure while creating, writing, syncing, or renaming.
    #[error("lock I/O at {path}")]
    Io {
        /// Lock file path associated with the failure.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },
}

/// Injectable clock and sleep for deterministic lock backoff tests.
pub trait LockClock: Send + Sync {
    /// Current instant (used for backoff deadlines).
    fn now(&self) -> Instant;
    /// Pause until the next acquisition attempt.
    fn sleep(&self, duration: Duration);
}

/// Injectable jitter so concurrent processes do not sleep in lockstep.
pub trait LockJitter: Send + Sync {
    /// Fraction in `[0.0, 1.0)` scaled into backoff delays.
    fn fraction(&self, lock_path: &Path, attempt: u32) -> f64;
}

/// OS random bytes for cross-process jitter (Git-style lock backoff).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemLockJitter;

impl LockJitter for SystemLockJitter {
    fn fraction(&self, _lock_path: &Path, _attempt: u32) -> f64 {
        let mut buf = [0u8; 8];
        if getrandom::getrandom(&mut buf).is_ok() {
            let n = u64::from_le_bytes(buf);
            (n % 10_000) as f64 / 10_000.0
        } else {
            0.5
        }
    }
}

/// Wall-clock backoff using [`std::thread::sleep`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemLockClock;

impl LockClock for SystemLockClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        if !duration.is_zero() {
            std::thread::sleep(duration);
        }
    }
}

/// Exclusive lock file created by this process.
///
/// Drop without [`Self::commit`] or [`Self::commit_to`] removes the lock file when this
/// handle created it. Foreign lock files are never removed.
pub struct LockFile {
    lock_path: PathBuf,
    commit_path: Option<PathBuf>,
    file: Option<File>,
    committed: bool,
}

impl LockFile {
    /// Create `target` exclusively (`O_CREAT|O_EXCL`), optionally waiting per `wait`.
    ///
    /// `target` is the lock file path (for example `refs/heads/main.lock`), not the file being
    /// updated. Persist staged bytes with [`Self::commit_to`] (or [`Self::commit`] after
    /// [`Self::with_commit_target`]).
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Held`] when the lock exists and the wait budget is exhausted, or
    /// [`LockError::Io`] for other failures.
    pub fn acquire(target: &Path, wait: LockWait) -> Result<Self, LockError> {
        Self::acquire_with_clock(
            target,
            wait,
            Arc::new(SystemLockClock),
            Arc::new(SystemLockJitter),
        )
    }

    /// Like [`Self::acquire`], with injectable clock and jitter for unit tests.
    pub fn acquire_with_clock(
        target: &Path,
        wait: LockWait,
        clock: Arc<dyn LockClock>,
        jitter: Arc<dyn LockJitter>,
    ) -> Result<Self, LockError> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: target.to_path_buf(),
                source,
            })?;
        }

        let deadline = match wait {
            LockWait::Immediately => Some(clock.now()),
            LockWait::Backoff { budget } => Some(clock.now() + budget),
            LockWait::Indefinitely => None,
        };

        let mut attempt: u32 = 0;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(target) {
                Ok(file) => {
                    return Ok(Self {
                        lock_path: target.to_path_buf(),
                        commit_path: None,
                        file: Some(file),
                        committed: false,
                    });
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    if matches!(wait, LockWait::Immediately)
                        || deadline.is_some_and(|deadline| clock.now() >= deadline)
                    {
                        return Err(LockError::Held {
                            path: target.to_path_buf(),
                        });
                    }
                    let delay = backoff_delay(target, attempt, jitter.as_ref());
                    attempt = attempt.saturating_add(1);
                    if let Some(deadline) = deadline {
                        let remaining = deadline.saturating_duration_since(clock.now());
                        clock.sleep(delay.min(remaining));
                    } else {
                        clock.sleep(delay);
                    }
                }
                Err(source) => {
                    return Err(LockError::Io {
                        path: target.to_path_buf(),
                        source,
                    });
                }
            }
        }
    }

    /// Set the destination path used by [`Self::commit`].
    #[must_use]
    pub fn with_commit_target(mut self, path: &Path) -> Self {
        self.commit_path = Some(path.to_path_buf());
        self
    }

    /// Path to the lock file held by this handle.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Borrow the open lock file for writing.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Io`] when the handle was already taken (for example after
    /// the open file handle was moved out for index integration).
    pub fn as_file(&self) -> Result<&File, LockError> {
        self.file.as_ref().ok_or_else(|| LockError::Io {
            path: self.lock_path.clone(),
            source: io::Error::new(io::ErrorKind::NotConnected, "lock file handle missing"),
        })
    }

    /// Borrow the open lock file mutably for writing.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Io`] when the handle was already taken.
    pub fn as_file_mut(&mut self) -> Result<&mut File, LockError> {
        self.file.as_mut().ok_or_else(|| LockError::Io {
            path: self.lock_path.clone(),
            source: io::Error::new(io::ErrorKind::NotConnected, "lock file handle missing"),
        })
    }

    /// Yield the lock path and open file without removing the lock on drop.
    pub(crate) fn into_open_file(mut self) -> Result<(PathBuf, File), LockError> {
        self.committed = true;
        let file = self.file.take().ok_or_else(|| LockError::Io {
            path: self.lock_path.clone(),
            source: io::Error::new(io::ErrorKind::NotConnected, "lock file handle missing"),
        })?;
        Ok((self.lock_path.clone(), file))
    }

    /// Fsync the lock file, rename it over `commit_path` from [`Self::with_commit_target`], and
    /// disarm drop cleanup.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Io`] when no commit target was configured or rename fails.
    pub fn commit(self) -> Result<(), LockError> {
        let dest = self.commit_path.clone().ok_or_else(|| LockError::Io {
            path: self.lock_path.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "lock commit target not configured",
            ),
        })?;
        self.commit_to(&dest)
    }

    /// Fsync the lock file and atomically rename it over `other_path`.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Io`] on sync or rename failure.
    pub fn commit_to(mut self, other_path: &Path) -> Result<(), LockError> {
        let file = self.file.take().ok_or_else(|| LockError::Io {
            path: self.lock_path.clone(),
            source: io::Error::new(io::ErrorKind::NotConnected, "lock file handle missing"),
        })?;
        file.sync_all().map_err(|source| LockError::Io {
            path: self.lock_path.clone(),
            source,
        })?;
        drop(file);
        fs::rename(&self.lock_path, other_path).map_err(|source| LockError::Io {
            path: self.lock_path.clone(),
            source,
        })?;
        self.committed = true;
        Ok(())
    }

    /// Release the lock without committing (same as drop).
    pub fn rollback(mut self) {
        if self.committed {
            return;
        }
        drop(self.file.take());
        let _ = fs::remove_file(&self.lock_path);
        self.committed = true;
    }

    /// Close this handle but leave the lock file on disk (mutex-only locks).
    pub fn detach(mut self) {
        self.committed = true;
    }
}

impl Write for LockFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.file.as_mut() {
            Some(file) => file.write(buf),
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "lock file handle missing",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "lock file handle missing",
            )),
        }
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        drop(self.file.take());
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn backoff_delay(lock_path: &Path, attempt: u32, jitter: &dyn LockJitter) -> Duration {
    const INITIAL_MS: u64 = 1;
    const CAP_MS: u64 = 50;
    let shift = attempt.min(16);
    let base_ms = (INITIAL_MS.saturating_mul(1_u64 << shift)).min(CAP_MS);
    let span_ms = base_ms / 2 + 1;
    let jitter_ms = (jitter.fraction(lock_path, attempt) * span_ms as f64) as u64;
    Duration::from_millis(base_ms.saturating_add(jitter_ms))
}

/// Process recorded as the owner of a lock file.
///
/// With `core.lockfilePid` enabled, the lock owner writes `pid <n>` to a sidecar named after
/// the lock (`index.lock` → `index~pid.lock`). Git and grit share this layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockHolder {
    /// Process id read from the sidecar.
    pub pid: u64,
    /// Whether a process with [`Self::pid`] exists right now.
    ///
    /// `false` means the lock is stale. `true` is only a hint: the id may have been reused by
    /// an unrelated process.
    pub running: bool,
}

impl LockHolder {
    /// Read the holder recorded for `lock_path` and probe whether it is still running.
    ///
    /// `lock_path` is the lock file itself (for example `.git/index.lock`).
    ///
    /// Returns `None` when no pid sidecar exists or it does not contain a process id.
    #[must_use]
    pub fn read(lock_path: &Path) -> Option<Self> {
        let pid = read_lock_pid(&pid_path_for_lock(lock_path))?;
        Some(Self {
            pid,
            running: is_process_running(pid),
        })
    }
}

/// Path of the pid sidecar for `lock_path` (`name.lock` → `name~pid.lock`).
pub(crate) fn pid_path_for_lock(lock_path: &Path) -> PathBuf {
    let file_name = lock_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index.lock".to_owned());
    let base = file_name.strip_suffix(".lock").unwrap_or(&file_name);
    lock_path.with_file_name(format!("{base}~pid.lock"))
}

fn read_lock_pid(pid_path: &Path) -> Option<u64> {
    let raw = fs::read_to_string(pid_path).ok()?;
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("pid ")
        .unwrap_or(trimmed)
        .trim()
        .parse::<u64>()
        .ok()
}

fn is_process_running(pid: u64) -> bool {
    if pid == 0 || pid > i32::MAX as u64 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: `OpenProcess` is called with a numeric pid and a read-only access mask.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if handle.is_null() {
                return false;
            }
            CloseHandle(handle);
            true
        }
    }
    #[cfg(all(unix, target_os = "linux"))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        use rustix::io::Errno;
        use rustix::process::{test_kill_process, Pid};
        let Some(pid) = Pid::from_raw(pid as i32) else {
            return false;
        };
        match test_kill_process(pid) {
            Ok(()) => true,
            Err(Errno::SRCH) => false,
            Err(Errno::PERM) => true,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeLockClock {
        now: Mutex<Instant>,
        sleeps: Mutex<Vec<Duration>>,
    }

    impl FakeLockClock {
        fn new(start: Instant) -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(start),
                sleeps: Mutex::new(Vec::new()),
            })
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl LockClock for FakeLockClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.lock().unwrap().push(duration);
            *self.now.lock().unwrap() += duration;
        }
    }

    #[test]
    fn drop_without_commit_removes_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("resource.lock");
        {
            let _lock = LockFile::acquire(&lock, LockWait::Immediately).unwrap();
            assert!(lock.is_file());
        }
        assert!(!lock.exists());
    }

    #[test]
    fn commit_to_replaces_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("resource");
        let lock_path = dir.path().join("resource.lock");
        let mut lock = LockFile::acquire(&lock_path, LockWait::Immediately).unwrap();
        lock.write_all(b"payload\n").unwrap();
        lock.commit_to(&target).unwrap();
        assert!(!lock_path.exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "payload\n");
    }

    struct FixedJitter(f64);

    impl LockJitter for FixedJitter {
        fn fraction(&self, _lock_path: &Path, _attempt: u32) -> f64 {
            self.0
        }
    }

    #[test]
    fn held_when_budget_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("x.lock");
        fs::write(&lock_path, b"foreign").unwrap();
        let start = Instant::now();
        let clock = FakeLockClock::new(start);
        let err = match LockFile::acquire_with_clock(
            &lock_path,
            LockWait::Backoff {
                budget: Duration::from_millis(10),
            },
            clock.clone(),
            Arc::new(FixedJitter(0.5)),
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected lock held error"),
        };
        assert!(matches!(err, LockError::Held { .. }));
        assert!(lock_path.is_file());
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "foreign");
        assert!(!clock.sleeps().is_empty());
    }

    #[test]
    fn pid_sidecar_path_replaces_lock_suffix() {
        assert_eq!(
            pid_path_for_lock(Path::new("/r/.git/index.lock")),
            Path::new("/r/.git/index~pid.lock")
        );
        assert_eq!(
            pid_path_for_lock(Path::new("/r/.git/refs/heads/main.lock")),
            Path::new("/r/.git/refs/heads/main~pid.lock")
        );
        assert_eq!(
            pid_path_for_lock(Path::new("/r/odd")),
            Path::new("/r/odd~pid.lock")
        );
    }

    #[test]
    fn held_converts_to_typed_error_with_holder() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("index.lock");
        fs::write(&lock_path, b"").unwrap();
        fs::write(
            dir.path().join("index~pid.lock"),
            format!("pid {}\n", std::process::id()),
        )
        .unwrap();
        let err = LockFile::acquire(&lock_path, LockWait::Immediately)
            .err()
            .expect("held");
        match crate::error::Error::from(err) {
            crate::error::Error::LockHeld { path, holder } => {
                assert_eq!(path, lock_path);
                assert_eq!(
                    holder,
                    Some(LockHolder {
                        pid: u64::from(std::process::id()),
                        running: true,
                    })
                );
            }
            other => panic!("expected LockHeld, got {other:?}"),
        }
    }

    #[test]
    fn io_failure_converts_to_io_error() {
        let err = LockError::Io {
            path: PathBuf::from("x.lock"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert!(matches!(
            crate::error::Error::from(err),
            crate::error::Error::Io(e) if e.kind() == io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn rollback_removes_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("resource.lock");
        let lock_file = LockFile::acquire(&lock, LockWait::Immediately).unwrap();
        assert!(lock.is_file());
        lock_file.rollback();
        assert!(!lock.exists());
    }

    #[test]
    fn files_ref_lock_timeout_uses_milliseconds() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("core.filesreflocktimeout", "100")
            .expect("set timeout");
        assert_eq!(LockWait::parse_files_ref_lock_timeout_ms(Some(&cfg)), 100);
        match LockWait::files_ref_lock(Some(&cfg)) {
            LockWait::Backoff { budget } => {
                assert_eq!(budget, Duration::from_millis(100));
            }
            other => panic!("expected backoff, got {other:?}"),
        }
    }

    #[test]
    fn files_ref_lock_timeout_negative_retries_indefinitely() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("core.filesreflocktimeout", "-1")
            .expect("set timeout");
        assert!(matches!(
            LockWait::files_ref_lock(Some(&cfg)),
            LockWait::Indefinitely
        ));
    }

    #[test]
    fn indefinite_wait_succeeds_when_lock_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("x.lock");
        fs::write(&lock_path, b"foreign").unwrap();
        let clock = FakeLockClock::new(Instant::now());
        let lock_path_thread = lock_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            let _ = fs::remove_file(&lock_path_thread);
        });
        let _guard = LockFile::acquire_with_clock(
            &lock_path,
            LockWait::Indefinitely,
            clock,
            Arc::new(FixedJitter(0.0)),
        )
        .expect("indefinite wait should eventually acquire");
    }
}
