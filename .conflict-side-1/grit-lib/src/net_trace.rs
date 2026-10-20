//! Network operation tracing through [`crate::diagnostics::Trace::Network`].
//!
//! Enable tracing on a [`crate::repo::Repository`] with [`crate::environment::RepositoryOptions::network_trace`]
//! and wire the same diagnostic sink into fetch/push [`FetchOptions`](crate::transfer::FetchOptions).

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
