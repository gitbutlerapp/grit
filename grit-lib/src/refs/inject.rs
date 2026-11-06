//! Reflog failure and ref-commit pause injection for tests and `test-support`.
#![allow(missing_docs)]

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;

use crate::error::{Error, Result};
use crate::git_path::CanonicalizeExt;

mod reflog_fail_inject {
    use std::cell::RefCell;

    thread_local! { // hygiene: test-support-only reflog failure injection
        pub(super) static INJECT: RefCell<Option<String>> = const { RefCell::new(None) };
    }
}

/// Fail the next reflog append for `failed_ref` (or any ref when `None`).
pub fn set_reflog_fail(failed_ref: Option<&str>) {
    reflog_fail_inject::INJECT.with(|c| {
        *c.borrow_mut() = failed_ref.map(str::to_owned);
    });
}

pub(crate) fn reflog_fail(refname: &str) -> Result<()> {
    reflog_fail_inject::INJECT.with(|c| {
        if c.borrow().as_deref() == Some(refname) {
            return Err(Error::Io(std::io::Error::other("injected reflog failure")));
        }
        Ok(())
    })
}

mod branch_ref_pause_inject {
    use super::{Arc, Condvar, Mutex, OnceLock};

    pub struct PauseGate {
        pub wait: Mutex<bool>,
        pub cv: Condvar,
    }

    // hygiene: test-support-only branch-ref commit pause injection slot
    static PAUSE_BEFORE_COMMIT: OnceLock<Mutex<Option<Arc<PauseGate>>>> = OnceLock::new();

    pub fn pause_slot() -> &'static Mutex<Option<Arc<PauseGate>>> {
        PAUSE_BEFORE_COMMIT.get_or_init(|| Mutex::new(None))
    }
}

/// Synchronization gate used with [`set_branch_ref_pause_before_commit`].
pub type BranchRefPauseGate = Arc<branch_ref_pause_inject::PauseGate>;

#[must_use]
pub fn branch_ref_pause_gate() -> BranchRefPauseGate {
    Arc::new(branch_ref_pause_inject::PauseGate {
        wait: Mutex::new(false),
        cv: Condvar::new(),
    })
}

pub fn set_branch_ref_pause_before_commit(gate: Option<BranchRefPauseGate>) {
    *branch_ref_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = gate;
}

pub(crate) fn branch_ref_pause_before_commit() {
    let gate = branch_ref_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(gate) = gate {
        let mut wait = gate.wait.lock().unwrap_or_else(|e| e.into_inner());
        *wait = true;
        gate.cv.notify_all();
        while *wait {
            wait = gate.cv.wait(wait).unwrap_or_else(|e| e.into_inner());
        }
    }
}

mod ref_store_batch_pause_inject {
    use std::path::PathBuf;

    use super::{Condvar, Mutex, OnceLock};

    pub struct PauseGate {
        pub wait: Mutex<bool>,
        pub cv: Condvar,
    }

    pub type PauseSlot = Option<(usize, PathBuf, std::sync::Arc<PauseGate>)>;

    // hygiene: test-support-only ref-store batch pause injection slot
    static PAUSE_AFTER_REF_WRITE: OnceLock<Mutex<PauseSlot>> = OnceLock::new();

    pub fn pause_slot() -> &'static Mutex<PauseSlot> {
        PAUSE_AFTER_REF_WRITE.get_or_init(|| Mutex::new(None))
    }
}

/// Synchronization gate used with [`set_ref_store_batch_pause_after_apply`].
pub type RefStoreBatchPauseGate = Arc<ref_store_batch_pause_inject::PauseGate>;

#[must_use]
pub fn ref_store_batch_pause_gate() -> RefStoreBatchPauseGate {
    Arc::new(ref_store_batch_pause_inject::PauseGate {
        wait: Mutex::new(false),
        cv: Condvar::new(),
    })
}

pub fn set_ref_store_batch_pause_after_apply(
    apply_index: usize,
    git_dir: &Path,
    gate: Option<RefStoreBatchPauseGate>,
) {
    *ref_store_batch_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = gate.map(|g| (apply_index, git_dir.to_path_buf(), g));
}

struct RefStorePauseTestSerial {
    holder: Option<ThreadId>,
}

// hygiene: test-support-only serialization for ref-store batch pause tests
static REF_STORE_PAUSE_TEST_SERIAL: Mutex<RefStorePauseTestSerial> =
    Mutex::new(RefStorePauseTestSerial { holder: None });

/// Serialize ref-store batch pause injection tests and clear injection on drop.
pub struct RefStoreBatchPauseTestScope {
    _serial: MutexGuard<'static, RefStorePauseTestSerial>,
}

impl Default for RefStoreBatchPauseTestScope {
    fn default() -> Self {
        Self::new()
    }
}

impl RefStoreBatchPauseTestScope {
    pub fn new() -> Self {
        let mut serial = REF_STORE_PAUSE_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            serial.holder.is_none(),
            "ref_store_batch_pause_test_scope must not nest"
        );
        serial.holder = Some(std::thread::current().id());
        set_ref_store_batch_pause_after_apply(0, Path::new(""), None);
        Self { _serial: serial }
    }
}

impl Drop for RefStoreBatchPauseTestScope {
    fn drop(&mut self) {
        set_ref_store_batch_pause_after_apply(0, Path::new(""), None);
        self._serial.holder = None;
    }
}

#[must_use]
pub fn ref_store_batch_pause_test_scope() -> RefStoreBatchPauseTestScope {
    RefStoreBatchPauseTestScope::new()
}

pub(crate) fn ref_store_batch_pause_after_ref_write(idx: usize, git_dir: &Path) {
    let slot = ref_store_batch_pause_inject::pause_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some((target, target_dir, gate)) = slot {
        let same_repo = |a: &Path, b: &Path| {
            a == b
                || matches!((a.canonicalize_simplified(), b.canonicalize_simplified()), (Ok(a), Ok(b)) if a == b)
        };
        if idx != target || !same_repo(git_dir, &target_dir) {
            return;
        }
        let mut wait = gate.wait.lock().unwrap_or_else(|e| e.into_inner());
        *wait = true;
        gate.cv.notify_all();
        while *wait {
            wait = gate.cv.wait(wait).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct PauseInjectionOwner {
    holder: Option<ThreadId>,
}

// hygiene: test-support-only serialization for ref pause injection tests
static PAUSE_INJECTION_OWNER: Mutex<PauseInjectionOwner> =
    Mutex::new(PauseInjectionOwner { holder: None });

/// Serialize tests that install global ref pause injection hooks (parallel `cargo test` safe).
pub struct PauseInjectionTestScope {
    _owner: MutexGuard<'static, PauseInjectionOwner>,
}

impl Drop for PauseInjectionTestScope {
    fn drop(&mut self) {
        set_branch_ref_pause_before_commit(None);
        set_ref_store_batch_pause_after_apply(0, Path::new(""), None);
        self._owner.holder = None;
    }
}

#[must_use]
pub fn pause_injection_test_scope() -> PauseInjectionTestScope {
    let mut owner = PAUSE_INJECTION_OWNER
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        owner.holder.is_none(),
        "pause_injection_test_scope must not nest"
    );
    owner.holder = Some(std::thread::current().id());
    PauseInjectionTestScope { _owner: owner }
}

use std::sync::MutexGuard;
