//! A machine-wide budget of worker threads, shared by cooperating processes.
//!
//! Each pool sizes itself to every logical CPU, which is right for one process alone.
//! When several grit processes run at once (agents often start many), every one of
//! them starts a full pool and the machine runs many times more threads than it has
//! cores. Checkout writes suffer worst: their workers spend their time in the kernel
//! (`open`, `write`, `close`) contending for the filesystem, and eight concurrent
//! clones ran ten times slower than eight with `--threads 1`.
//!
//! With the budget enabled ([`share_worker_budget_across_processes`]), a pool leases
//! its workers from one slot per logical CPU, or, when it creates or removes files
//! ([`FilesystemWork`]), from a few slots kept for that. Slots are byte-range locks on a
//! per-user file in the temp directory, so every grit process of that user shares
//! them, and the kernel releases a process's slots when it exits, however it exits.
//! A pool that gets no slot still runs one worker, so work always progresses; a
//! process alone gets every slot.
//!
//! The budget is off unless enabled, so library embedders and tests keep the exact
//! worker counts they ask for.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

// hygiene: process-wide switch the CLI turns on once at startup
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Workers shared machine-wide by pools that create or remove files.
///
/// Those workers spend their time in the kernel, where file creation and removal
/// serialize (on APFS especially), so beyond a few of them more only adds contention.
/// On an 18-core Mac, 4 checkout writers cloned a 5,000-file repository as fast as 18,
/// and eight such clones at once took as long as eight with one writer each.
const FILESYSTEM_SLOTS: usize = 4;

// hygiene: marks the calling thread's pool as filesystem work for its slot choice
thread_local! {
    static FILESYSTEM_WORK: Cell<bool> = const { Cell::new(false) };
}

/// Share one budget of worker threads with other grit processes on this machine.
///
/// When enabled, parallel pools lease their workers from a machine-wide budget of
/// one slot per logical CPU (and a few slots for pools that create or remove
/// files), so many concurrent processes together run about as many workers as the
/// machine can use instead of each running a full pool. Where the budget cannot be
/// set up (no usable temp directory, or a platform without byte-range locks), pools
/// keep their configured sizes.
pub fn share_worker_budget_across_processes(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// While alive, pools started on this thread lease from the filesystem budget.
pub(crate) struct FilesystemWork {
    outer: bool,
}

impl FilesystemWork {
    /// Mark pools started on this thread as creating or removing files.
    pub(crate) fn enter() -> Self {
        Self {
            outer: FILESYSTEM_WORK.with(|w| w.replace(true)),
        }
    }
}

impl Drop for FilesystemWork {
    fn drop(&mut self) {
        FILESYSTEM_WORK.with(|w| w.set(self.outer));
    }
}

/// Workers a pool may run; their slots return to the budget on drop.
pub(crate) struct WorkerLease {
    workers: usize,
    #[cfg(unix)]
    slots: Option<(&'static unix::Slots, Vec<usize>)>,
}

impl WorkerLease {
    /// Number of workers the pool may run, counting the calling thread when it works.
    /// Always at least one.
    pub(crate) fn workers(&self) -> usize {
        self.workers
    }
}

/// Lease up to `want` workers for one pool (at least one, even when no slot is free).
pub(crate) fn lease_workers(want: usize) -> WorkerLease {
    let want = want.max(1);
    #[cfg(unix)]
    if want > 1 && ENABLED.load(Ordering::Relaxed) {
        if let Some(file) = unix::slot_file() {
            let slots = if FILESYSTEM_WORK.with(Cell::get) {
                &file.filesystem
            } else {
                &file.cpu
            };
            let held = file.acquire(slots, want);
            return WorkerLease {
                workers: held.len().max(1),
                slots: Some((slots, held)),
            };
        }
    }
    WorkerLease {
        workers: want,
        #[cfg(unix)]
        slots: None,
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some((slots, held)) = self.slots.take() {
            if let Some(file) = unix::slot_file() {
                file.release(slots, &held);
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;
    use std::sync::{Mutex, OnceLock};

    /// One class of slots: the bytes `base..base + held.len()` of the slot file.
    pub(crate) struct Slots {
        base: usize,
        /// Slots this process holds. `fcntl` locks belong to the process, so a second
        /// lock on a byte it already holds would succeed; this keeps two pools of one
        /// process from sharing a slot.
        pub(super) held: Mutex<Vec<bool>>,
    }

    /// The per-user slot file, held open for the life of the process.
    ///
    /// Closing any descriptor of a file drops every `fcntl` lock the process holds
    /// on it, so the file is opened once and never closed.
    pub(super) struct SlotFile {
        file: File,
        pub(super) cpu: Slots,
        pub(super) filesystem: Slots,
    }

    pub(super) fn slot_file() -> Option<&'static SlotFile> {
        // hygiene: the per-user slot file, opened once per process
        static SLOTS: OnceLock<Option<SlotFile>> = OnceLock::new();
        SLOTS.get_or_init(open_slot_file).as_ref()
    }

    fn open_slot_file() -> Option<SlotFile> {
        // SAFETY: `getuid` has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let path = std::env::temp_dir().join(format!("grit-worker-slots-{uid}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        // A file someone else made in a shared temp directory is not ours to trust.
        let meta = file.metadata().ok()?;
        if meta.uid() != uid || !meta.is_file() {
            return None;
        }
        let cpus = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        Some(SlotFile {
            file,
            cpu: Slots {
                base: 0,
                held: Mutex::new(vec![false; cpus]),
            },
            // Far past any CPU count, so processes that see different counts agree.
            filesystem: Slots {
                base: 1 << 20,
                held: Mutex::new(vec![false; super::FILESYSTEM_SLOTS.min(cpus)]),
            },
        })
    }

    impl SlotFile {
        /// Take up to `want` free slots of `slots` without waiting; returns their indices.
        pub(super) fn acquire(&self, slots: &Slots, want: usize) -> Vec<usize> {
            let mut held = slots.held.lock().unwrap_or_else(|e| e.into_inner());
            let mut got = Vec::new();
            for slot in 0..held.len() {
                if got.len() == want {
                    break;
                }
                if !held[slot] && self.set_lock(slots.base + slot, i32::from(libc::F_WRLCK)) {
                    held[slot] = true;
                    got.push(slot);
                }
            }
            got
        }

        pub(super) fn release(&self, slots: &Slots, indices: &[usize]) {
            let mut held = slots.held.lock().unwrap_or_else(|e| e.into_inner());
            for &slot in indices {
                self.set_lock(slots.base + slot, i32::from(libc::F_UNLCK));
                held[slot] = false;
            }
        }

        /// Take or drop the one-byte lock at `offset` without waiting.
        fn set_lock(&self, offset: usize, kind: i32) -> bool {
            // SAFETY: an all-zero `flock` is a valid value; the fields that matter are set below.
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = kind as libc::c_short;
            lock.l_whence = libc::SEEK_SET as libc::c_short;
            lock.l_start = offset as libc::off_t;
            lock.l_len = 1;
            // SAFETY: the descriptor stays open for the life of `self`, and `lock` is a valid
            // `flock` that outlives the call.
            unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_SETLK, &lock) == 0 }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn leases_never_overlap_and_return_their_slots() {
        let Some(file) = unix::slot_file() else {
            return;
        };
        let a = file.acquire(&file.cpu, 3);
        let b = file.acquire(&file.cpu, 3);
        assert!(a.iter().all(|s| !b.contains(s)), "{a:?} overlaps {b:?}");
        file.release(&file.cpu, &a);
        {
            let held = file.cpu.held.lock().unwrap_or_else(|e| e.into_inner());
            assert!(a.iter().all(|&s| !held[s]), "released slots are free again");
        }
        file.release(&file.cpu, &b);
    }

    #[test]
    fn disabled_budget_grants_every_worker_asked_for() {
        assert_eq!(lease_workers(0).workers(), 1);
        assert_eq!(lease_workers(7).workers(), 7);
        let _fs = FilesystemWork::enter();
        assert_eq!(lease_workers(7).workers(), 7);
    }

    #[test]
    fn filesystem_work_scope_nests_and_restores() {
        let inner_flag = || FILESYSTEM_WORK.with(Cell::get);
        assert!(!inner_flag());
        {
            let _outer = FilesystemWork::enter();
            {
                let _inner = FilesystemWork::enter();
                assert!(inner_flag());
            }
            assert!(inner_flag());
        }
        assert!(!inner_flag());
    }
}
