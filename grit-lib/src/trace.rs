//! Git-style trace output (`GIT_TRACE`, `GIT_TRACE_SETUP`) without reading process env.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// Sink for Git trace lines (stderr, a file path, or disabled).
#[derive(Clone, Debug, Default)]
pub struct TraceSink {
    /// Raw `GIT_TRACE` value when tracing is enabled (`1`, `2`, `true`, or a file path).
    pub git_trace: Option<String>,
}

impl TraceSink {
    /// Build from an optional trace setting (typically from [`crate::environment::Environment`]).
    #[must_use]
    pub fn from_git_trace(raw: Option<&str>) -> Self {
        Self {
            git_trace: raw.map(str::to_owned),
        }
    }

    /// Whether trace output is enabled for the given raw value.
    #[must_use]
    pub fn is_enabled(raw: Option<&str>) -> bool {
        let Some(trace_val) = raw else {
            return false;
        };
        if trace_val.is_empty() || trace_val == "0" || trace_val.eq_ignore_ascii_case("false") {
            return false;
        }
        true
    }

    /// Emit one trace line when tracing is enabled.
    pub fn trace(&self, line: &str) {
        let Some(trace_val) = self.git_trace.as_deref() else {
            return;
        };
        if !Self::is_enabled(Some(trace_val)) {
            return;
        }
        let payload = if line.ends_with('\n') {
            line.to_owned()
        } else {
            format!("{line}\n")
        };
        match trace_val {
            "1" | "true" | "2" => {
                let _ = std::io::stderr().write_all(payload.as_bytes());
            }
            path => {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = f.write_all(payload.as_bytes());
                }
            }
        }
    }
}

/// Append a `setup:` trace line when `GIT_TRACE_SETUP` names an absolute path (Git test format).
pub fn append_trace_setup(path: &Path, line: &str) {
    if !path.is_absolute() {
        return;
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "setup: {line}");
    }
}
