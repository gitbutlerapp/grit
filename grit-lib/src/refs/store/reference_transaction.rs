//! `reference-transaction` hook around ref store prepare/commit.

use std::sync::Arc;

use crate::hooks::{RefHookContext, ReferenceTransactionPhase};
use crate::objects::ObjectId;

use super::{PreparedRefTransaction, RawRef, RefStore, RefStoreError, RefTransaction, RefUpdate};

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// One `<old-hex> <new-hex> <ref>` line for the hook stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReferenceTransactionLine {
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub refname: String,
}

impl ReferenceTransactionLine {
    fn format_line(&self) -> String {
        format!(
            "{} {} {}\n",
            self.old_oid.to_hex(),
            self.new_oid.to_hex(),
            self.refname
        )
    }
}

/// Build sorted hook stdin lines for `updates` against current `store` state.
pub(crate) fn reference_transaction_lines(
    store: &dyn RefStore,
    updates: &[RefUpdate],
    null_oid: ObjectId,
) -> StoreResult<Vec<ReferenceTransactionLine>> {
    let mut lines = Vec::new();
    for update in updates {
        if let Some(line) = line_for_update(store, update, null_oid)? {
            lines.push(line);
        }
    }
    lines.sort_by(|a, b| a.refname.cmp(&b.refname));
    Ok(lines)
}

fn line_for_update(
    store: &dyn RefStore,
    update: &RefUpdate,
    null_oid: ObjectId,
) -> StoreResult<Option<ReferenceTransactionLine>> {
    if update.flags.log_only {
        let Some(RawRef::Direct(new_oid)) = &update.new_value else {
            return Ok(None);
        };
        let old_oid = current_direct_oid(store, &update.name, null_oid)?;
        return Ok(Some(ReferenceTransactionLine {
            old_oid,
            new_oid: *new_oid,
            refname: update.name.clone(),
        }));
    }
    match &update.new_value {
        None => {
            let old_oid = current_direct_oid(store, &update.name, null_oid)?;
            Ok(Some(ReferenceTransactionLine {
                old_oid,
                new_oid: null_oid,
                refname: update.name.clone(),
            }))
        }
        Some(RawRef::Direct(new_oid)) => {
            let old_oid = current_direct_oid(store, &update.name, null_oid)?;
            Ok(Some(ReferenceTransactionLine {
                old_oid,
                new_oid: *new_oid,
                refname: update.name.clone(),
            }))
        }
        Some(RawRef::Symbolic(_)) => Ok(None),
    }
}

fn current_direct_oid(
    store: &dyn RefStore,
    name: &str,
    null_oid: ObjectId,
) -> StoreResult<ObjectId> {
    let Some(raw) = store
        .read_raw(name)
        .map_err(|e| RefStoreError::Corrupt(e.to_string()))?
    else {
        return Ok(null_oid);
    };
    match raw {
        RawRef::Direct(oid) => Ok(oid),
        RawRef::Symbolic(_) => Ok(null_oid),
    }
}

pub(crate) fn stdin_from_lines(lines: &[ReferenceTransactionLine]) -> Vec<u8> {
    lines
        .iter()
        .flat_map(|l| l.format_line().into_bytes())
        .collect()
}

pub(crate) fn run_reference_transaction_phase(
    ctx: &RefHookContext,
    phase: ReferenceTransactionPhase,
    lines: &[ReferenceTransactionLine],
) -> StoreResult<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let stdin = stdin_from_lines(lines);
    ctx.run_reference_transaction(phase, &stdin)
        .map_err(|e| RefStoreError::Hook(e.into()))
}

/// Prepared batch with optional hook phases after the backend locks.
pub(crate) struct HookWrappedPrepared<'a> {
    inner: Option<Box<dyn PreparedRefTransaction + Send + 'a>>,
    hooks: Arc<RefHookContext>,
    lines: Vec<ReferenceTransactionLine>,
    hook_armed: bool,
}

impl<'a> HookWrappedPrepared<'a> {
    pub(crate) fn new(
        inner: Box<dyn PreparedRefTransaction + Send + 'a>,
        hooks: Arc<RefHookContext>,
        lines: Vec<ReferenceTransactionLine>,
    ) -> Self {
        Self {
            inner: Some(inner),
            hooks,
            lines,
            hook_armed: true,
        }
    }
}

impl Drop for HookWrappedPrepared<'_> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let _ = inner.abort();
        }
        if self.hook_armed {
            let _ = run_reference_transaction_phase(
                self.hooks.as_ref(),
                ReferenceTransactionPhase::Aborted,
                &self.lines,
            );
        }
    }
}

impl PreparedRefTransaction for HookWrappedPrepared<'_> {
    fn rejected_updates(&self) -> &[(String, RefStoreError)] {
        match self.inner.as_ref() {
            Some(inner) => inner.rejected_updates(),
            None => &[],
        }
    }

    fn commit(mut self: Box<Self>) -> StoreResult<()> {
        self.hook_armed = false;
        let inner = self
            .inner
            .take()
            .ok_or_else(|| RefStoreError::Corrupt("ref transaction already finished".into()))?;
        inner.commit()?;
        let _ = run_reference_transaction_phase(
            self.hooks.as_ref(),
            ReferenceTransactionPhase::Committed,
            &self.lines,
        );
        Ok(())
    }

    fn abort(mut self: Box<Self>) -> StoreResult<()> {
        self.hook_armed = false;
        let inner = self.inner.take();
        let result = inner.map_or(Ok(()), |inner| inner.abort());
        let _ = run_reference_transaction_phase(
            self.hooks.as_ref(),
            ReferenceTransactionPhase::Aborted,
            &self.lines,
        );
        result
    }
}

/// Run prepare on `inner`, then `prepared` / `committed` / `aborted` hooks when `hooks` is set.
pub(crate) fn prepare_with_reference_transaction_hooks<'a>(
    store: &'a dyn RefStore,
    inner: &'a dyn RefStore,
    hooks: Option<&'a Arc<RefHookContext>>,
    null_oid: ObjectId,
    txn: RefTransaction,
) -> StoreResult<Box<dyn PreparedRefTransaction + Send + 'a>> {
    let updates: Vec<RefUpdate> = txn.updates().to_vec();
    let inner_prepared = inner.prepare(txn)?;
    let Some(hooks) = hooks else {
        return Ok(inner_prepared);
    };
    let lines = reference_transaction_lines(store, &updates, null_oid)?;
    if let Err(err) =
        run_reference_transaction_phase(hooks.as_ref(), ReferenceTransactionPhase::Prepared, &lines)
    {
        let _ = inner_prepared.abort();
        let _ = run_reference_transaction_phase(
            hooks.as_ref(),
            ReferenceTransactionPhase::Aborted,
            &lines,
        );
        return Err(err);
    }
    Ok(Box::new(HookWrappedPrepared::new(
        inner_prepared,
        Arc::clone(hooks),
        lines,
    )))
}

/// Hook lines for a files-backend branch commit (matches Git's `HEAD` + branch pair).
pub(crate) fn branch_commit_reference_transaction_lines(
    branch_ref: &str,
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> Vec<ReferenceTransactionLine> {
    let mut lines = vec![
        ReferenceTransactionLine {
            old_oid,
            new_oid,
            refname: "HEAD".to_owned(),
        },
        ReferenceTransactionLine {
            old_oid,
            new_oid,
            refname: branch_ref.to_owned(),
        },
    ];
    lines.sort_by(|a, b| a.refname.cmp(&b.refname));
    lines
}

/// Hook lines for detached `HEAD` commit on the files backend.
pub(crate) fn detached_head_commit_reference_transaction_lines(
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> Vec<ReferenceTransactionLine> {
    vec![ReferenceTransactionLine {
        old_oid,
        new_oid,
        refname: "HEAD".to_owned(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::zero_oid;
    use crate::refs::store::{MemoryRefStore, RefUpdateFlags};

    fn oid(byte: u8) -> ObjectId {
        let mut b = [0u8; 20];
        b[19] = byte;
        ObjectId::from_bytes(&b).expect("oid")
    }

    #[test]
    fn skips_symbolic_only_update() {
        let store = MemoryRefStore::new();
        store
            .set_ref("HEAD", RawRef::Symbolic("refs/heads/main".into()))
            .expect("head");
        let null = zero_oid();
        let lines = reference_transaction_lines(
            &store,
            &[RefUpdate {
                name: "HEAD".into(),
                new_value: Some(RawRef::Symbolic("refs/heads/other".into())),
                expected: crate::refs::store::Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            }],
            null,
        )
        .expect("lines");
        assert!(lines.is_empty());
    }

    #[test]
    fn log_only_direct_includes_head() {
        let store = MemoryRefStore::new();
        let old = oid(1);
        let new = oid(2);
        store.set_ref("HEAD", RawRef::Direct(old)).expect("head");
        let null = zero_oid();
        let lines = reference_transaction_lines(
            &store,
            &[RefUpdate {
                name: "HEAD".into(),
                new_value: Some(RawRef::Direct(new)),
                expected: crate::refs::store::Expected::Any,
                reflog: None,
                flags: RefUpdateFlags {
                    log_only: true,
                    no_deref: true,
                    ..Default::default()
                },
            }],
            null,
        )
        .expect("lines");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].old_oid, old);
        assert_eq!(lines[0].new_oid, new);
    }
}
