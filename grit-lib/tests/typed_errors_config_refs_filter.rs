//! Typed errors for config, refs, filters (task 502 acceptance).

use grit_lib::config::ConfigSet;
use grit_lib::config::{ConfigFile, ConfigScope};
use grit_lib::crlf::{convert_to_git_with_opts, ConversionConfig, ConvertToGitOpts, FileAttrs};
use grit_lib::error::{
    ApplyError, BadNumericSource, ConfigError, Error, FilterError, FilterPhase, RefLockError,
};
use grit_lib::objects::ObjectId;
use grit_test_support::{git, git_cmd};
use std::fs;
use tempfile::tempdir;

#[test]
fn malformed_config_line_is_bad_config_line_typed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("broken");
    let content = "[section]\n\t!!!not a valid config entry\n";
    fs::write(&path, content).unwrap();
    let err = ConfigFile::parse(&path, content, ConfigScope::Local).unwrap_err();
    assert!(matches!(
        err,
        Error::Config(ConfigError::BadConfigLine { .. })
            | Error::Config(ConfigError::BadConfigLineInFile { .. })
    ));
}

#[test]
fn non_numeric_diff_context_is_bad_numeric_value() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config");
    fs::write(&path, "[diff]\n\tcontext = not-a-number\n").unwrap();
    let file = ConfigFile::parse(
        &path,
        &fs::read_to_string(&path).unwrap(),
        ConfigScope::Local,
    )
    .expect("parse");
    let mut set = grit_lib::config::ConfigSet::new();
    set.merge(&file);
    let err = grit_lib::config::resolve_diff_context_lines(&set).unwrap_err();
    match err {
        Error::Config(ConfigError::BadNumericValue { key, value, reason }) => {
            assert_eq!(key, "diff.context");
            assert_eq!(value, "not-a-number");
            assert_eq!(reason, BadNumericSource::InvalidUnit);
        }
        Error::Config(ConfigError::BadNumericValueInFile {
            key, value, reason, ..
        }) => {
            assert_eq!(key, "diff.context");
            assert_eq!(value, "not-a-number");
            assert_eq!(reason, BadNumericSource::InvalidUnit);
        }
        other => panic!("expected bad numeric diff.context, got {other:?}"),
    }
}

#[test]
fn ref_lock_directory_in_the_way_matches_git_update_ref() {
    let dir = tempdir().unwrap();
    let init = git_cmd(&["init"]).in_dir(dir.path()).exec();
    assert!(init.ok(), "git init failed: {}", init.stderr);
    git(
        dir.path(),
        &["commit", "--allow-empty", "-q", "-m", "seed object for lock test"],
    );
    let head = git(dir.path(), &["rev-parse", "HEAD"]);
    let oid = ObjectId::from_hex(head.trim()).expect("HEAD oid");
    let git_dir = dir.path().join(".git");
    fs::create_dir_all(git_dir.join("refs/heads/feature")).unwrap();
    fs::write(
        git_dir.join("refs/heads/feature/extra"),
        "0000000000000000000000000000000000000000\n",
    )
    .unwrap();
    let err = grit_lib::refs::write_ref(&git_dir, "refs/heads/feature", &oid).unwrap_err();
    assert!(matches!(
        err,
        Error::RefLock(RefLockError::DirectoryInTheWay { .. })
    ));

    let git_out = git_cmd(&["update-ref", "refs/heads/feature", &oid.to_hex()])
        .in_dir(dir.path())
        .exec();
    assert!(!git_out.ok(), "git should refuse when directory blocks ref");
    let combined = format!("{}{}", git_out.stderr, git_out.stdout);
    assert!(
        combined.contains("blocking reference")
            || combined.contains("cannot lock ref")
            || combined.contains("exists"),
        "git should refuse the ref update: {combined}"
    );
}

#[test]
fn required_clean_filter_failure_is_typed_message() {
    let conv = ConversionConfig::from_config(&ConfigSet::new());
    let mut attrs = FileAttrs::default();
    attrs.filter_clean = Some("false".to_owned());
    attrs.filter_driver_name = Some("testfilter".to_owned());
    attrs.filter_clean_required = true;
    let err = convert_to_git_with_opts(
        b"data",
        "file.txt",
        &conv,
        &attrs,
        ConvertToGitOpts {
            check_safecrlf: false,
            renormalize: false,
            index_blob: None,
            filter_process: None,
            command_runner: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        FilterError::ExternalFilterFailed {
            driver: "testfilter".to_owned(),
            path: "file.txt".to_owned(),
            phase: FilterPhase::Clean,
        }
    );
}

#[test]
fn diagnostic_lines_interpolate_dynamic_values() {
    let p = "hooks/pre-commit";
    let msg = "Permission denied";
    let line = grit_lib::diagnostics::error_line(&format!("cannot exec '{p}': {msg}"));
    assert!(line.contains("hooks/pre-commit"));
    assert!(line.contains("Permission denied"));
    assert!(!line.contains("{p}"));
    assert!(!line.contains("{msg}"));

    let name = "foo/bar";
    let warn =
        grit_lib::diagnostics::warning_line(&format!("ignoring suspicious submodule name: {name}"));
    assert!(warn.contains("foo/bar"));
    assert!(!warn.contains("{name}"));
}

#[test]
fn bom_filter_error_has_single_fatal_prefix_on_stderr() {
    let conv = ConversionConfig::from_config(&ConfigSet::new());
    let mut attrs = FileAttrs::default();
    attrs.working_tree_encoding = Some("UTF-16LE".to_owned());
    // Big-endian BOM is prohibited when the attribute requests UTF-16LE.
    let data = [0xFE, 0xFF, 0x00, 0x41];
    let filter_err = convert_to_git_with_opts(
        &data,
        "utf-bom.txt",
        &conv,
        &attrs,
        ConvertToGitOpts {
            check_safecrlf: true,
            renormalize: false,
            index_blob: None,
            filter_process: None,
            command_runner: None,
        },
    )
    .unwrap_err();
    assert!(matches!(
        filter_err,
        FilterError::NotFilteredProperly { .. }
    ));
    let stderr = Error::Filter(filter_err).git_stderr_message();
    assert_eq!(
        stderr.matches("fatal:").count(),
        1,
        "expected one fatal prefix, got: {stderr}"
    );
    assert!(
        !stderr.contains("fatal: utf-bom.txt: fatal:"),
        "duplicated fatal prefix: {stderr}"
    );
    assert!(stderr.contains("BOM is prohibited"));
}

#[test]
fn corrupt_patch_surfaces_apply_error() {
    use grit_lib::apply::parse_patch;
    let input = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n one\n";
    let err = parse_patch(input, 1, "patch", false, None).unwrap_err();
    match &err {
        Error::Apply(ApplyError::CorruptPatch { line, .. }) => assert!(*line >= 3),
        other => panic!("expected corrupt patch, got {other:?}"),
    }
}
