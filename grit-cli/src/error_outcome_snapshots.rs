//! Golden snapshots of structured `--json` error objects (error.v1.json).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_test_support::ModeCapture;
use serde::Serialize;

use crate::json_error::{StructuredJsonError, WouldOverwriteJson};

fn snapshot_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"))
}

#[derive(Serialize)]
struct ErrorModesSnapshot<'a> {
    human: &'a ModeCapture,
    json: &'a ModeCapture,
    markdown: &'a ModeCapture,
}

fn snapshot_structured_error(stem: &str, payload: StructuredJsonError, human: &str) {
    let json = serde_json::to_string(&payload).expect("serialize error json");
    let markdown = format!("## Error\n\n- **{}** — {}", payload.kind, payload.error);
    let exit = 1;
    let human_cap = ModeCapture {
        exit,
        stdout: String::new(),
        stderr: human.to_owned(),
    };
    let json_cap = ModeCapture {
        exit,
        stdout: format!("{json}\n"),
        stderr: String::new(),
    };
    let md_cap = ModeCapture {
        exit,
        stdout: markdown,
        stderr: String::new(),
    };
    let payload = ErrorModesSnapshot {
        human: &human_cap,
        json: &json_cap,
        markdown: &md_cap,
    };
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path(snapshot_dir());
    settings.set_prepend_module_to_snapshot(false);
    settings.bind(|| {
        insta::assert_json_snapshot!(format!("grit_test_support__cli_fixture__{stem}"), payload);
    });
}

#[test]
fn error_usage_snapshot() {
    snapshot_structured_error(
        "error_usage",
        StructuredJsonError {
            error: "branch delete requires a branch name".to_owned(),
            kind: "usage",
            conflicts: None,
            commit: None,
            would_overwrite: None,
            oid: None,
        },
        "error: branch delete requires a branch name",
    );
}

#[test]
fn error_invalid_pattern_snapshot() {
    snapshot_structured_error(
        "error_invalid_pattern",
        StructuredJsonError {
            error: "invalid commit filter pattern `[`: unterminated bracket expression".to_owned(),
            kind: "invalid_pattern",
            conflicts: None,
            commit: None,
            would_overwrite: None,
            oid: None,
        },
        "error: invalid commit filter pattern `[`: unterminated bracket expression",
    );
}

#[test]
fn error_would_overwrite_snapshot() {
    snapshot_structured_error(
        "error_would_overwrite",
        StructuredJsonError {
            error: "switch would overwrite local changes".to_owned(),
            kind: "would_overwrite",
            conflicts: None,
            commit: None,
            would_overwrite: Some(WouldOverwriteJson {
                reason: "local_changes".to_owned(),
                paths: vec!["tracked.txt".to_owned()],
            }),
            oid: None,
        },
        "error: switch would overwrite local changes",
    );
}

#[test]
fn error_conflict_snapshot() {
    let human = "history move has conflicts in:\n  tracked.txt\n\nNothing was changed. grit can't resolve conflicts yet.";
    snapshot_structured_error(
        "error_conflict",
        StructuredJsonError {
            error: "history move has conflicts".to_owned(),
            kind: "conflict",
            conflicts: Some(vec!["tracked.txt".to_owned()]),
            commit: None,
            would_overwrite: None,
            oid: None,
        },
        human,
    );
}
