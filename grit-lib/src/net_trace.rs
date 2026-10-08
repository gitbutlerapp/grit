//! Lightweight, env-gated tracing for the networking paths (transport connect,
//! fetch/push negotiation, pack transfer).
//!
//! Set `GRIT_NET_DEBUG=1` to print one-line `[grit-net] …` markers to stderr
//! before, during, and after each remote operation. Off by default and
//! essentially free when disabled (the gate is read once and the format args are
//! not evaluated). Embedders can consult [`enabled_for`] to interleave their own
//! before/after markers with the library's.

use std::sync::OnceLock;

use crate::environment::Environment;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Install the global net-trace gate from an [`Environment`] (typically at fetch/push entry).
pub fn configure_from_environment(env: &Environment) {
    let _ = ENABLED.set(env.grit_net_debug_enabled());
}

/// Whether networking trace output is enabled for `env`.
#[must_use]
pub fn enabled_for(env: &Environment) -> bool {
    env.grit_net_debug_enabled()
}

/// Whether networking trace output is enabled. Requires prior [`configure_from_environment`].
#[must_use]
pub fn enabled() -> bool {
    *ENABLED.get().unwrap_or(&false)
}

/// Emit one `[grit-net] …` trace line to stderr (only when [`enabled`]).
///
/// Prefer the `net_trace!` macro at call sites so the format arguments are
/// skipped entirely when tracing is off.
pub fn line(msg: &str) {
    eprintln!("[grit-net] {msg}");
}

/// Emit an env-gated `[grit-net]` trace line. No-op (and no formatting) unless
/// networking trace is enabled.
macro_rules! net_trace {
    ($($arg:tt)*) => {
        if $crate::net_trace::enabled() {
            $crate::net_trace::line(&format!($($arg)*));
        }
    };
}

pub(crate) use net_trace;
