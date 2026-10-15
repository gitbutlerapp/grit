//! NFD argv pathspecs match NFC index paths when precompose is enabled (issue #1007).

use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{
    matches_pathspec_list_precompose, precompose_argv_pathspec,
    resolve_pathspec_with_argv_precompose,
};
use grit_lib::{config::environment::RepositoryOptions, index::Index};
use grit_test_support::{grit_cmd, unique_tmp};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn enable_precomposeunicode(root: &Path) {
    let config_path = root.join(".git/config");
    let mut config = fs::read_to_string(&config_path).expect("read config");
    if !config.contains("precomposeunicode") {
        config.push_str("\n[core]\n\tprecomposeunicode = true\n");
        fs::write(&config_path, config).expect("write precomposeunicode");
    }
}

#[test]
fn library_nfd_resolved_pathspec_stages_nfc_index_entry() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    grit_cmd(&["init"])
        .in_dir(root)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc();
    enable_precomposeunicode(root);

    let nfc_name = "caf\u{00e9}.txt";
    let nfd_name = format!("cafe\u{0301}.txt");
    fs::write(root.join(nfc_name), b"one\n").expect("write file");

    grit_cmd(&["commit", "-a", "-m", "init"])
        .in_dir(root)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc();

    // On Linux the NFD and NFC spellings are distinct paths; modify the tracked NFC file.
    fs::write(root.join(nfc_name), b"one\nmore\n").expect("modify tracked file");

    let repo =
        Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover repo");
    let wt = repo.work_tree().expect("work tree");
    let nfd_resolved = resolve_pathspec_with_argv_precompose(&nfd_name, wt, None, true);

    let outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: vec![nfd_resolved],
            pathspec_sources: vec![nfd_name.clone()],
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut grit_lib::progress::NullProgress,
    )
    .expect("stage nfd pathspec");

    assert!(
        outcome.total() > 0,
        "expected staging to update the modified NFC path"
    );

    let index = Index::load(&root.join(".git/index")).expect("load index");
    assert!(
        index
            .entries()
            .iter()
            .any(|e| e.path.as_slice() == b"caf\xc3\xa9.txt"),
        "index should still store NFC spelling"
    );
}

#[test]
fn precompose_pathspec_matching_unit() {
    let nfc = "caf\u{00e9}.txt";
    let nfd = format!("cafe\u{0301}.txt");
    assert!(matches_pathspec_list_precompose(nfc, &[nfd.clone()], true));
    assert_eq!(precompose_argv_pathspec(&nfd).as_ref(), nfc);
}

#[test]
fn grit_add_nfd_pathspec_reports_no_match_when_file_missing() {
    let scratch = unique_tmp("nfd-pathspec", "missing");
    fs::create_dir_all(&scratch).expect("scratch");
    grit_cmd(&["init"])
        .in_dir(&scratch)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc();
    enable_precomposeunicode(&scratch);

    let nfd = format!("cafe\u{0301}.txt");
    let out = grit_cmd(&["add", &nfd])
        .in_dir(&scratch)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .exec();
    assert!(
        !out.ok(),
        "grit add should fail when NFD pathspec matches nothing: {}",
        out.dump("grit add")
    );
    assert!(
        out.stderr.contains("did not match") || out.stderr.contains("match any"),
        "expected pathspec no-match error, got: {}",
        out.stderr
    );
}
