//! Cancel in-flight smart-protocol RPC work when a request is dropped.

use std::sync::{Arc, Mutex, Weak};

use grit_lib::progress::{Cancel, CancelToken};

/// Tracks [`CancelToken`] handles for in-flight receive/upload RPC work.
#[derive(Default)]
pub(crate) struct RpcCancelRegistry {
    inner: Arc<RpcCancelRegistryInner>,
}

#[derive(Default)]
struct RpcCancelRegistryInner {
    active: Mutex<Vec<Weak<CancelToken>>>,
}

impl RpcCancelRegistry {
    /// Register a cancel flag for one RPC and return it for [`ServeSession`].
    pub(crate) fn register(&self) -> (Arc<dyn Cancel + Send + Sync>, RpcCancelGuard) {
        let cancel = Arc::new(CancelToken::new());
        self.inner
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(&cancel));
        let session: Arc<dyn Cancel + Send + Sync> = cancel.clone();
        (
            session,
            RpcCancelGuard {
                cancel,
                registry: self.inner.clone(),
                completed: false,
            },
        )
    }

    pub(crate) fn cancel_all(&self) {
        let mut active = self.inner.active.lock().unwrap_or_else(|e| e.into_inner());
        for weak in active.drain(..) {
            if let Some(c) = weak.upgrade() {
                c.cancel();
            }
        }
    }

    fn unregister(inner: &RpcCancelRegistryInner, cancel: &Arc<CancelToken>) {
        let mut active = inner.active.lock().unwrap_or_else(|e| e.into_inner());
        active.retain(|weak| weak.upgrade().is_none_or(|c| !Arc::ptr_eq(&c, cancel)));
    }
}

/// Cancels the RPC when dropped unless [`Self::complete`] ran first.
pub(crate) struct RpcCancelGuard {
    cancel: Arc<CancelToken>,
    registry: Arc<RpcCancelRegistryInner>,
    completed: bool,
}

impl RpcCancelGuard {
    pub(crate) fn mark_finished(mut self) {
        self.completed = true;
        RpcCancelRegistry::unregister(&self.registry, &self.cancel);
    }
}

impl Drop for RpcCancelGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        RpcCancelRegistry::unregister(&self.registry, &self.cancel);
        self.cancel.cancel();
    }
}
