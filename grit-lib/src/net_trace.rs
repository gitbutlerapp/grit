//! Network operation tracing through [`crate::diagnostics::Trace::Network`].
//!
//! Enable tracing on a [`crate::repo::Repository`] with [`RepositoryOptions::network_trace`]
//! and wire the same diagnostic sink into fetch/push [`FetchOptions`](crate::transfer::FetchOptions).

use crate::diagnostics::{DiagnosticsHandle, Trace};

/// Emit a network trace line when `enabled` and `sink` are both set.
pub fn trace_optional(enabled: bool, sink: Option<&DiagnosticsHandle>, message: String) {
    if enabled {
        if let Some(s) = sink {
            s.trace(Trace::Network { message });
        }
    }
}

/// Emit a network trace when `enabled` and `sink` are both set. Skips formatting when off.
macro_rules! net_trace {
    ($enabled:expr, $sink:expr, $($arg:tt)*) => {
        if $enabled {
            if let Some(s) = $sink {
                s.trace($crate::diagnostics::Trace::Network {
                    message: format!($($arg)*),
                });
            }
        }
    };
}

pub(crate) use net_trace;
