//! Exclusive lock files with optional Git-style acquisition backoff.
//!
//! Grit uses lock files (`.lock` siblings, `index.lock`, `packed-refs.lock`, …) the same way
//! Git does: create exclusively, write staged content, then rename over the target path.
//! [`LockFile::acquire`] retries with jittered exponential backoff when configured, matching
//! `core.filesRefLockTimeout`, `core.packedRefsTimeout`, and `reftable.lockTimeout`.

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
}

impl LockWait {
    /// `core.filesRefLockTimeout` (microseconds), default 100 ms when unset.
    #[must_use]
    pub fn files_ref_lock(config: Option<&ConfigSet>) -> Self {
        Self::from_microseconds_config(config, "core.filesreflocktimeout", 100_000)
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

    fn from_microseconds_config(
        config: Option<&ConfigSet>,
        key: &str,
        default_micros: u64,
    ) -> Self {
        let micros = config
            .and_then(|cfg| cfg.get(key))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map(|v| v.max(0) as u64)
            .unwrap_or(default_micros);
        if micros == 0 {
            Self::Immediately
        } else {
            Self::Backoff {
                budget: Duration::from_micros(micros),
            }
        }
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
    #[allow(dead_code)]
    clock: Arc<dyn LockClock>,
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
        Self::acquire_with_clock(target, wait, Arc::new(SystemLockClock))
    }

    /// Like [`Self::acquire`], with an injectable clock for unit tests.
    pub fn acquire_with_clock(
        target: &Path,
        wait: LockWait,
        clock: Arc<dyn LockClock>,
    ) -> Result<Self, LockError> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: target.to_path_buf(),
                source,
            })?;
        }

        let deadline = match wait {
            LockWait::Immediately => clock.now(),
            LockWait::Backoff { budget } => clock.now() + budget,
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
                        clock,
                    });
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    if matches!(wait, LockWait::Immediately) || clock.now() >= deadline {
                        return Err(LockError::Held {
                            path: target.to_path_buf(),
                        });
                    }
                    let delay = backoff_delay(attempt);
                    attempt = attempt.saturating_add(1);
                    let remaining = deadline.saturating_duration_since(clock.now());
                    clock.sleep(delay.min(remaining));
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
    /// Returns [`LockError::Io`] when the handle was already taken (for example by
    /// [`Self::into_open_file`]).
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

fn backoff_delay(attempt: u32) -> Duration {
    const INITIAL_MICROS: u64 = 1_000;
    const CAP_MICROS: u64 = 50_000;
    let shift = attempt.min(16);
    let base = (INITIAL_MICROS.saturating_mul(1_u64 << shift)).min(CAP_MICROS);
    let jitter = pseudo_jitter_us(attempt);
    Duration::from_micros(base.saturating_add(jitter % (base / 2 + 1)))
}

fn pseudo_jitter_us(attempt: u32) -> u64 {
    let x = attempt.wrapping_mul(0x9E37_79B9);
    (x as u64).wrapping_mul(1_103) % 10_000
}

/// Map [`LockError::Held`] to a ref-update `AlreadyExists` I/O error (legacy call sites).
pub fn lock_held_as_ref_present(err: LockError) -> io::Error {
    match err {
        LockError::Held { path } => io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("ref lock present: {}", path.display()),
        ),
        LockError::Io { source, .. } => source,
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
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected lock held error"),
        };
        assert!(matches!(err, LockError::Held { .. }));
        assert!(lock_path.is_file());
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "foreign");
        assert!(!clock.sleeps().is_empty());
    }
}
