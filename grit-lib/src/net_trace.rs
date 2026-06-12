//! Lightweight, env-gated tracing for the networking paths (transport connect,
//! fetch/push negotiation, pack transfer).
//!
//! Set `GRIT_NET_DEBUG=1` to print one-line `[grit-net] …` markers to stderr
//! before, during, and after each remote operation. Off by default and
//! essentially free when disabled (the gate is read once and the format args are
//! not evaluated). Embedders can consult [`enabled`] to interleave their own
//! before/after markers with the library's.

use std::sync::OnceLock;

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
/// Prefer the [`net_trace!`] macro at call sites so the format arguments are
/// skipped entirely when tracing is off.
pub fn line(msg: &str) {
    eprintln!("[grit-net] {msg}");
}

/// Emit an env-gated `[grit-net]` trace line. No-op (and no formatting) unless
/// `GRIT_NET_DEBUG` is set.
macro_rules! net_trace {
    ($($arg:tt)*) => {
        if $crate::net_trace::enabled() {
            $crate::net_trace::line(&format!($($arg)*));
        }
    };
}

pub(crate) use net_trace;
