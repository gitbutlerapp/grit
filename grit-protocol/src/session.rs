//! Per-request session controls for smart-protocol handlers.

use std::sync::Arc;

use grit_lib::progress::{Cancel, NeverCancel};
use grit_lib::remote::serve::ReceiveHook;

/// Hooks and cancellation for one upload-pack or receive-pack exchange.
#[derive(Clone, Default)]
pub struct ServeSession {
    /// In-process receive-pack checks alongside [`crate::PushGuard`].
    pub receive_hook: Option<Arc<dyn ReceiveHook + Send + Sync>>,
    /// When set, long-running serve work stops without moving refs on push.
    pub cancel: Option<Arc<dyn Cancel + Send + Sync>>,
}

impl ServeSession {
    /// No extra hook and no cancel signal.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) -> &dyn Cancel {
        match self.cancel.as_deref() {
            Some(c) => c as &dyn Cancel,
            None => &NeverCancel as &dyn Cancel,
        }
    }
}

/// Buffered receive-pack response plus the typed session outcome.
#[derive(Debug)]
pub struct ReceiveRpcResult {
    /// The HTTP or smart-HTTP response body (report-status).
    pub body: Vec<u8>,
    /// What the push stored and which updates were applied.
    pub outcome: grit_lib::remote::serve::ReceiveOutcome,
}
