//! Typed errors for config, refs, filters (task 502 acceptance).

use grit_lib::config::ConfigSet;
use grit_lib::config::{ConfigFile, ConfigScope};
use grit_lib::crlf::{convert_to_git_with_opts, ConversionConfig, ConvertToGitOpts, FileAttrs};
use grit_lib::error::{ApplyError, ConfigError, Error, RefLockError};
use grit_lib::objects::ObjectId;
use grit_test_support::git_cmd;
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
    assert!(matches!(
        err,
        Error::Config(
            ConfigError::BadNumericValue { .. } | ConfigError::BadNumericValueInFile { .. }
        )
    ));
}

#[test]
fn ref_lock_directory_in_the_way_matches_git_update_ref() {
    let dir = tempdir().unwrap();
    git_cmd(&["init"]).in_dir(dir.path()).exec();
    let git_dir = dir.path().join(".git");
    fs::create_dir_all(git_dir.join("refs/heads/feature")).unwrap();
    fs::write(
        git_dir.join("refs/heads/feature/extra"),
        "0000000000000000000000000000000000000000\n",
    )
    .unwrap();
    let oid = ObjectId::from_hex("67bf698f3ab735e92fb011a99cff3497c44d30c1").unwrap();
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
        },
    )
    .unwrap_err();
    assert!(err.contains("clean filter"));
    assert!(err.contains("file.txt"));
    assert!(!err.contains("fatal:"));
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
