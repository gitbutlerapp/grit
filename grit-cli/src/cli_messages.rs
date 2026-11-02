//! Map library and structured errors to short CLI messages.

use anyhow::Error;
use grit_lib::error::Error as LibError;
use grit_lib::refs::store::RefStoreError;

use crate::json_error::StructuredCliError;
use crate::revision_errors::map_lib_revision_error;

/// Human-readable (and JSON `error` string) message for a command failure.
pub fn human_error_message(err: &Error) -> String {
    if let Some(structured) = err.downcast_ref::<StructuredCliError>() {
        return structured.human.clone();
    }
    if let Some(lib) = err.downcast_ref::<LibError>() {
        return map_lib_error(lib);
    }
    for cause in err.chain() {
        if let Some(structured) = cause.downcast_ref::<StructuredCliError>() {
            return structured.human.clone();
        }
        if let Some(
            lib
            @ (LibError::HashCollision { .. } | LibError::LockHeld { .. } | LibError::Config(_)),
        ) = cause.downcast_ref::<LibError>()
        {
            return map_lib_error(lib);
        }
    }
    format!("{err:#}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use grit_lib::lockfile::LockHolder;

    use super::*;
    use crate::error_kind::ErrorKind;
    use crate::json_error::{ensure_structured, structured_json_payload};

    /// Library error carried as the `source()` of another error type, so `anyhow` downcasts
    /// cannot see it and only a chain walk finds it.
    #[derive(Debug)]
    struct Wrapper(LibError);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("wrapped operation failed")
        }
    }

    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    fn wrapped_lock_held(holder: Option<LockHolder>) -> (Error, PathBuf) {
        let path = PathBuf::from("/repo/.git/index.lock");
        let err = Error::new(Wrapper(LibError::LockHeld {
            path: path.clone(),
            holder,
        }))
        .context("staging files");
        (err, path)
    }

    #[test]
    fn nested_lock_held_gets_lock_prose_in_human_message() {
        let (err, path) = wrapped_lock_held(Some(LockHolder {
            pid: 42,
            running: false,
        }));
        assert!(err.downcast_ref::<LibError>().is_none());
        let human = human_error_message(&err);
        assert!(human.contains(&path.display().to_string()), "{human}");
        assert!(human.contains("stale lock"), "{human}");
        assert!(human.contains("process 42"), "{human}");
    }

    #[test]
    fn context_wrapped_config_error_keeps_config_message() {
        use grit_lib::error::{ConfigError, Error as LibError};
        let err = Error::new(LibError::Config(ConfigError::BadConfigLineInFile {
            file: ".git/config".into(),
            line: 6,
        }))
        .context("not in a repository");
        let human = human_error_message(&err);
        assert!(human.contains("bad config line"), "{human}");
        assert!(!human.contains("not in a repository"), "{human}");
        assert!(human.starts_with("fatal:"), "{human}");
    }

    #[test]
    fn ensure_structured_rewraps_nested_lock_held() {
        let (err, path) = wrapped_lock_held(None);
        let structured = ensure_structured(err);
        assert!(structured.downcast_ref::<StructuredCliError>().is_some());
        let human = human_error_message(&structured);
        assert!(human.contains("another process holds"), "{human}");
        let payload = structured_json_payload(&structured).expect("payload");
        assert_eq!(payload.kind, ErrorKind::LockHeld);
        assert_eq!(payload.error, human);
        assert_eq!(payload.lock.expect("lock").path, path.display().to_string());
    }
}

fn map_lib_error(err: &LibError) -> String {
    if let Some(msg) = map_lib_revision_error(err) {
        return msg;
    }
    match err {
        LibError::HashCollision { oid } => {
            format!(
                "refusing to store content that triggers an SHA-1 hash collision (digest {})",
                oid.to_hex()
            )
        }
        LibError::LockHeld { path, holder } => {
            crate::json_error::lock_held(path, *holder).to_string()
        }
        LibError::RefStore(RefStoreError::NotFound { name }) => {
            match name.strip_prefix("refs/heads/") {
                Some(short) => format!("no branch named '{short}'"),
                None => format!("ref not found: {name}"),
            }
        }
        LibError::InvalidPathComponent { path, reason } => {
            format!("cannot use path '{path}': {reason}")
        }
        LibError::Hook(hook) => {
            let mut message = hook.to_string();
            let mut cause = std::error::Error::source(hook);
            while let Some(c) = cause {
                message.push_str(": ");
                message.push_str(&c.to_string());
                cause = c.source();
            }
            message
        }
        other => other.git_stderr_message(),
    }
}
