//! Integration tests for the diagnostics sink wiring.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diagnostics::{CollectingDiagnostics, Warning};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
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
    let options = RepositoryOptions::empty()
        .with_diagnostics(sink.clone())
        .with_network_trace(false)
        .with_work_tree(tmp.path().to_path_buf());
    let repo = Repository::open(&tmp.path().join(".git"), &options).expect("open");

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

#[test]
fn core_bare_with_worktree_reaches_each_fresh_sink() {
    let tmp = TempDir::new().expect("tempdir");
    let bare = tmp.path().join("repo.git");
    git(&tmp, &["init", "--bare", bare.to_str().expect("utf8")]);
    let wt = tmp.path().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(
        &tmp,
        &[
            "config",
            "-f",
            bare.join("config").to_str().expect("utf8"),
            "core.bare",
            "true",
        ],
    );
    git(
        &tmp,
        &[
            "config",
            "-f",
            bare.join("config").to_str().expect("utf8"),
            "core.worktree",
            wt.to_str().expect("utf8"),
        ],
    );

    let open = |sink: Arc<CollectingDiagnostics>| {
        let options = RepositoryOptions::empty()
            .with_diagnostics(sink.clone())
            .with_network_trace(false);
        Repository::open(&bare, &options).expect("open");
        sink
    };

    let sink_a = Arc::new(CollectingDiagnostics::new());
    open(sink_a.clone());
    assert!(
        sink_a
            .warnings()
            .iter()
            .any(|w| matches!(w, Warning::CoreBareWithWorktree)),
        "first sink: {:?}",
        sink_a.warnings()
    );

    let sink_b = Arc::new(CollectingDiagnostics::new());
    open(sink_b.clone());
    assert!(
        sink_b
            .warnings()
            .iter()
            .any(|w| matches!(w, Warning::CoreBareWithWorktree)),
        "second sink should also receive warning, got {:?}",
        sink_b.warnings()
    );
}

#[test]
fn mailmap_blob_naming_a_tree_warns_and_still_loads_other_sources() {
    use grit_lib::config::mailmap::{load_mailmap_table, read_mailmap_blob};
    use grit_lib::objects::ObjectKind;

    let tmp = TempDir::new().expect("tempdir");
    git(&tmp, &["init", "-q"]);
    git(&tmp, &["config", "user.email", "a@b.c"]);
    git(&tmp, &["config", "user.name", "A"]);
    std::fs::write(
        tmp.path().join(".mailmap"),
        "Proper <proper@example.com> <old@example.com>\n",
    )
    .unwrap();
    git(&tmp, &["add", ".mailmap"]);
    git(&tmp, &["commit", "-q", "-m", "c1"]);
    git(&tmp, &["config", "mailmap.blob", "HEAD^{tree}"]);

    let sink = Arc::new(CollectingDiagnostics::new());
    let options = RepositoryOptions::empty()
        .with_diagnostics(sink.clone())
        .with_network_trace(false)
        .with_work_tree(tmp.path().to_path_buf());
    let repo = Repository::open(&tmp.path().join(".git"), &options).expect("open");

    let table = load_mailmap_table(&repo).expect("non-blob mailmap.blob is not fatal");
    let (name, email) = table.map_user("Old".into(), "old@example.com".into());
    assert_eq!(
        (name.as_str(), email.as_str()),
        ("Proper", "proper@example.com")
    );
    assert_eq!(
        sink.warnings(),
        vec![Warning::MailmapUnreadable {
            spec: "HEAD^{tree}".to_owned(),
            kind: ObjectKind::Tree,
        }]
    );

    assert!(read_mailmap_blob(&repo, "HEAD^{tree}").is_err());
    assert_eq!(
        read_mailmap_blob(&repo, "HEAD:.mailmap").expect("blob"),
        "Proper <proper@example.com> <old@example.com>\n"
    );

    sink.clear();
    git(&tmp, &["config", "mailmap.blob", "no-such-rev"]);
    repo.reload_config().expect("reload");
    load_mailmap_table(&repo).expect("unresolvable mailmap.blob is skipped");
    assert!(sink.warnings().is_empty(), "{:?}", sink.warnings());
}
