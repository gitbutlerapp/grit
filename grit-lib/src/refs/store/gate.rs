//! FIFO admission for ref stores with one in-process writer slot.
//!
//! [`SingleWriterGate`] serializes `prepare` / `commit` so threads updating disjoint refs
//! queue fairly instead of losing to jittered exponential backoff between fail-fast
//! [`super::RefStoreError::LockHeld`] retries.

use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::lockfile::LockWait;

use super::RefStoreError;

/// Fair FIFO gate for a single in-process writer slot.
///
/// Embed one gate per store (or per writer domain). Call [`Self::acquire`] from
/// [`super::RefStore::prepare_for`], hold the returned [`SingleWriterTicket`] until
/// [`super::PreparedRefTransaction::commit`] or abort (including `Drop`), then let the
/// next waiter proceed.
#[derive(Debug, Default)]
pub struct SingleWriterGate {
    inner: Mutex<GateInner>,
    cvar: Condvar,
}

#[derive(Debug, Default)]
struct GateInner {
    next_ticket: u64,
    serving: u64,
    abandoned: BTreeSet<u64>,
}

/// Holds the writer slot until dropped (commit, abort, or a failed prepare after acquire).
#[derive(Debug)]
#[must_use = "dropping releases the writer slot to the next FIFO waiter"]
pub struct SingleWriterTicket<'a> {
    gate: &'a SingleWriterGate,
    released: bool,
}

impl SingleWriterGate {
    /// Create an idle gate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn skip_abandoned(state: &mut GateInner) {
        while state.abandoned.remove(&state.serving) {
            state.serving = state.serving.saturating_add(1);
        }
    }

    /// Wait up to `wait` to reach the front of the queue, then take the writer slot.
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::LockHeld`] when the deadline elapses before this ticket is
    /// served. Abandoned tickets are skipped so the queue cannot stall.
    pub fn acquire<'a>(
        &'a self,
        wait: LockWait,
        lock_name: &str,
    ) -> Result<SingleWriterTicket<'a>, RefStoreError> {
        let deadline = match wait {
            LockWait::Immediately => Some(Instant::now()),
            LockWait::Backoff { budget } => Some(Instant::now() + budget),
            LockWait::Indefinitely => None,
        };

        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.saturating_add(1);
        drop(state);

        loop {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            Self::skip_abandoned(&mut state);

            if state.serving == ticket {
                drop(state);
                return Ok(SingleWriterTicket {
                    gate: self,
                    released: false,
                });
            }

            if matches!(wait, LockWait::Immediately) {
                state.abandoned.insert(ticket);
                Self::skip_abandoned(&mut state);
                drop(state);
                self.cvar.notify_all();
                return Err(RefStoreError::LockHeld {
                    name: lock_name.to_owned(),
                });
            }

            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                state.abandoned.insert(ticket);
                Self::skip_abandoned(&mut state);
                drop(state);
                self.cvar.notify_all();
                return Err(RefStoreError::LockHeld {
                    name: lock_name.to_owned(),
                });
            }

            let timeout = deadline.map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_micros(1))
            });
            state = match timeout {
                Some(timeout) => {
                    self.cvar
                        .wait_timeout(state, timeout)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.cvar.wait(state).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    fn release(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.serving = state.serving.saturating_add(1);
        Self::skip_abandoned(&mut state);
        drop(state);
        self.cvar.notify_all();
    }
}

impl Drop for SingleWriterTicket<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.gate.release();
            self.released = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn gate_serves_waiters_in_order() {
        let gate = Arc::new(SingleWriterGate::new());
        let first = gate
            .acquire(LockWait::Immediately, "refs/heads/a")
            .expect("first");
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done2 = Arc::clone(&done);
        let gate2 = Arc::clone(&gate);
        let waiter = thread::spawn(move || {
            let _t = gate2
                .acquire(
                    LockWait::Backoff {
                        budget: Duration::from_secs(1),
                    },
                    "refs/heads/b",
                )
                .expect("second");
            done2.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        thread::sleep(Duration::from_millis(20));
        assert!(!done.load(std::sync::atomic::Ordering::Relaxed));
        drop(first);
        waiter.join().expect("join");
        assert!(done.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn gate_timeout_skips_ticket_and_next_proceeds() {
        let gate = Arc::new(SingleWriterGate::new());
        let _first = gate
            .acquire(LockWait::Immediately, "refs/heads/a")
            .expect("holder");

        let gate2 = Arc::clone(&gate);
        let timed = thread::spawn(move || {
            gate2
                .acquire(
                    LockWait::Backoff {
                        budget: Duration::from_millis(30),
                    },
                    "refs/heads/b",
                )
                .map(|_| ())
        });
        let result = timed.join().expect("join");
        assert!(matches!(
            result,
            Err(RefStoreError::LockHeld { name }) if name == "refs/heads/b"
        ));

        drop(_first);
        let third = gate
            .acquire(
                LockWait::Backoff {
                    budget: Duration::from_millis(50),
                },
                "refs/heads/c",
            )
            .expect("third after skipped waiter");
        drop(third);
    }
}
