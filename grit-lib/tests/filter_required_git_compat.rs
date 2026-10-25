//! Clean/smudge filter failures: `filter.<driver>.required` never stores unfiltered content,
//! optional filters fall back to it like Git, and spawn failures keep their cause (#1810).

use std::fs;
use std::io;
use std::path::Path;

use grit_lib::command_runner::{CommandRunner, CommandSpec, RunningCommand};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::error::{Error, FilterError, FilterPhase};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::worktree::crlf::{
    convert_to_git_with_opts, convert_to_worktree_with_runner, ConversionConfig, ConvertToGitOpts,
    FileAttrs,
};
use grit_test_support::{git, git_cmd};

/// A runner on a machine with no shell: every spawn fails with `NotFound`.
struct NoShellRunner;

impl CommandRunner for NoShellRunner {
    fn spawn(&self, _spec: &CommandSpec) -> io::Result<RunningCommand> {
        Err(io::Error::new(io::ErrorKind::NotFound, "program not found"))
    }
}

fn filter_attrs(required: bool) -> FileAttrs {
    let mut attrs = FileAttrs::default();
    attrs.filter_driver_name = Some("up".to_owned());
    attrs.filter_clean = Some("tr a-z A-Z".to_owned());
    attrs.filter_smudge = Some("tr A-Z a-z".to_owned());
    attrs.filter_clean_required = required;
    attrs.filter_smudge_required = required;
    attrs
}

fn clean_opts(runner: &dyn CommandRunner) -> ConvertToGitOpts<'_> {
    ConvertToGitOpts {
        check_safecrlf: false,
        command_runner: Some(runner),
        ..ConvertToGitOpts::default()
    }
}

fn expected_spawn_error(phase: FilterPhase) -> FilterError {
    FilterError::ExternalFilterSpawnFailed {
        driver: "up".to_owned(),
        path: "y.up".to_owned(),
        phase,
        program: "sh".to_owned(),
        kind: io::ErrorKind::NotFound,
        detail: "program not found".to_owned(),
    }
}

#[test]
fn required_clean_filter_that_cannot_spawn_reports_the_cause() {
    let conv = ConversionConfig::from_config(&ConfigSet::new());
    let err = convert_to_git_with_opts(
        b"abc\n",
        "y.up",
        &conv,
        &filter_attrs(true),
        clean_opts(&NoShellRunner),
    )
    .expect_err("required clean filter must not fall back to raw content");
    assert_eq!(err, expected_spawn_error(FilterPhase::Clean));
    let message = err.to_string();
    assert!(
        message.contains("y.up") && message.contains("sh"),
        "{message}"
    );
    assert!(message.contains("program not found"), "{message}");
}

#[test]
fn required_smudge_filter_that_cannot_spawn_reports_the_cause() {
    let conv = ConversionConfig::from_config(&ConfigSet::new());
    let err = convert_to_worktree_with_runner(
        b"ABC\n",
        "y.up",
        &conv,
        &filter_attrs(true),
        None,
        None,
        None,
        None,
        Some(&NoShellRunner),
    )
    .expect_err("required smudge filter must fail");
    assert_eq!(err, expected_spawn_error(FilterPhase::Smudge));
}

#[test]
fn required_process_filter_that_cannot_spawn_reports_the_cause() {
    let mut conv = ConversionConfig::from_config(&ConfigSet::new());
    conv.command_runner = std::sync::Arc::new(NoShellRunner);
    let mut attrs = FileAttrs::default();
    attrs.filter_driver_name = Some("up".to_owned());
    attrs.filter_process = Some("git-lfs filter-process".to_owned());
    attrs.filter_clean_required = true;
    let err = convert_to_git_with_opts(
        b"abc\n",
        "y.up",
        &conv,
        &attrs,
        ConvertToGitOpts {
            check_safecrlf: false,
            ..ConvertToGitOpts::default()
        },
    )
    .expect_err("required process filter must fail");
    assert_eq!(err, expected_spawn_error(FilterPhase::Clean));
}

#[test]
fn optional_filters_fall_back_to_unfiltered_content() {
    let conv = ConversionConfig::from_config(&ConfigSet::new());
    let attrs = filter_attrs(false);
    let cleaned =
        convert_to_git_with_opts(b"abc\n", "y.up", &conv, &attrs, clean_opts(&NoShellRunner))
            .expect("optional clean filter falls back");
    assert_eq!(cleaned, b"abc\n");

    let mut failing = attrs.clone();
    failing.filter_clean = Some("exit 3".to_owned());
    let system = grit_lib::command_runner::SystemCommandRunner;
    let cleaned = convert_to_git_with_opts(b"abc\n", "y.up", &conv, &failing, clean_opts(&system))
        .expect("optional failing clean filter falls back");
    assert_eq!(cleaned, b"abc\n");

    let smudged = convert_to_worktree_with_runner(
        b"ABC\n",
        "y.up",
        &conv,
        &attrs,
        None,
        None,
        None,
        None,
        Some(&NoShellRunner),
    )
    .expect("optional smudge filter falls back");
    assert_eq!(smudged.as_deref(), Some(&b"ABC\n"[..]));
}

fn filtered_repo(root: &Path, clean: &str) -> Repository {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "filter.up.clean", clean]);
    git(root, &["config", "filter.up.smudge", "tr A-Z a-z"]);
    git(root, &["config", "filter.up.required", "true"]);
    fs::write(root.join(".gitattributes"), "*.up filter=up\n").expect("attributes");
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo")
}

#[test]
fn failing_required_clean_filter_fails_stage_and_leaves_index_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = filtered_repo(root, "exit 3");
    fs::write(root.join("y.up"), "abc\n").expect("write");

    let err = stage(&repo, &StageOptions::default(), &mut NullProgress)
        .expect_err("stage must fail when a required clean filter fails");
    assert!(
        matches!(
            &err,
            Error::Filter(FilterError::ExternalFilterFailed { driver, path, phase: FilterPhase::Clean })
                if driver == "up" && path == "y.up"
        ),
        "{err:?}"
    );
    assert!(
        git(root, &["ls-files", "--stage"]).trim().is_empty(),
        "nothing may be staged after a required filter failure"
    );
    assert!(
        !git_cmd(&["add", "y.up"]).in_dir(root).exec().ok(),
        "git add fails the same way"
    );
}

#[test]
fn working_filter_round_trips_with_git_and_status_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = filtered_repo(root, "tr a-z A-Z");
    fs::write(root.join("x.up"), "abc\n").expect("write");
    git(root, &["add", "x.up", ".gitattributes"]);
    git(root, &["commit", "-qm", "base"]);
    fs::write(root.join("y.up"), "def\n").expect("write");

    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    assert_eq!(git(root, &["cat-file", "-p", ":y.up"]), "DEF\n");
    git(root, &["fsck", "--strict"]);
    assert!(
        git(root, &["status", "--porcelain"])
            .lines()
            .all(|l| l == "A  y.up"),
        "git sees only the staged addition"
    );

    // Racy-clean entries make status hash the worktree through the clean filter.
    fs::remove_file(root.join(".git/index")).expect("drop index");
    git(root, &["read-tree", "HEAD"]);
    git(root, &["add", "y.up"]);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.is_empty(),
        "filtered files must not show as modified: {:?}",
        model.unstaged
    );
}
