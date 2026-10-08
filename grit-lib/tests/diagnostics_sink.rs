//! Integration tests for the diagnostics sink wiring.

use grit_lib::diagnostics::{CollectingDiagnostics, DiagnosticSink, Warning};
use grit_lib::repo::{Repository, RepositoryOptions};
use grit_lib::rev_parse::resolve_revision;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn git(repo: &TempDir, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo.path())
        .status()
        .expect("run git");
    assert!(status.success(), "git {:?} failed", args);
}

#[test]
fn ambiguous_refname_reaches_collecting_sink() {
    let tmp = TempDir::new().expect("tempdir");
    git(&tmp, &["init"]);
    git(&tmp, &["config", "user.email", "a@b.c"]);
    git(&tmp, &["config", "user.name", "A"]);
    std::fs::write(tmp.path().join("f"), "x").unwrap();
    git(&tmp, &["add", "f"]);
    git(&tmp, &["commit", "-m", "c1"]);
    std::fs::write(tmp.path().join("g"), "y").unwrap();
    git(&tmp, &["add", "g"]);
    git(&tmp, &["commit", "-m", "c2"]);
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .expect("rev-parse");
    let head_hex = String::from_utf8(head.stdout)
        .expect("utf8")
        .trim()
        .to_string();
    let prefix = head_hex[..7].to_string();
    let first = Command::new("git")
        .args(["rev-parse", "HEAD~1"])
        .current_dir(tmp.path())
        .output()
        .expect("rev-parse parent");
    let first_hex = String::from_utf8(first.stdout)
        .expect("utf8")
        .trim()
        .to_string();
    git(&tmp, &["branch", "-f", &prefix, &first_hex]);

    let sink = Arc::new(CollectingDiagnostics::new());
    let options = RepositoryOptions {
        diagnostics: sink.clone(),
        network_trace: false,
    };
    let repo = Repository::open_with_options(&tmp.path().join(".git"), Some(tmp.path()), options)
        .expect("open");

    let _ = resolve_revision(&repo, &prefix).expect("resolve");
    assert!(
        sink.warnings().iter().any(|w| matches!(
            w,
            Warning::AmbiguousRefname { spec } if spec == &prefix
        )),
        "expected AmbiguousRefname for {prefix}, got {:?}",
        sink.warnings()
    );
}
