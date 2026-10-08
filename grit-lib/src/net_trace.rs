//! Network operation tracing through [`crate::diagnostics::Trace::Network`].
//!
//! Enable tracing on a [`crate::repo::Repository`] with [`RepositoryOptions::network_trace`]
//! and wire the same diagnostic sink into fetch/push [`FetchOptions`](crate::transfer::FetchOptions).

use std::sync::OnceLock;

use crate::diagnostics::{DiagnosticsHandle, Trace};

// hygiene: immutable env flag cache (set once from GritNetDebug)
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether networking trace output is enabled (`GRIT_NET_DEBUG` set to something
/// other than empty / `0` / `false`). Read once and cached.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| match std::env::var("GRIT_NET_DEBUG") {
        Ok(v) => !v.is_empty() && v != "0" && v != "false",
        Err(_) => false,
    })
}

/// Emit one `[grit-net] …` trace line to stderr (only when [`enabled`]).
///
/// Prefer the `net_trace!` macro at call sites so the format arguments are
/// skipped entirely when tracing is off.
pub fn line(msg: &str) {
    eprintln!("[grit-net] {msg}");
}

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
