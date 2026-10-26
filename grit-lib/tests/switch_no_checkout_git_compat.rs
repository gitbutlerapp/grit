//! Initial switch materialization, including upstream t2018's no-checkout branch scenario.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::{Error, OverwriteReason};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions, SwitchOutcome};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::Repository;
use grit_lib::PostOperationHooks;
use grit_test_support::git;

fn source() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("source");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "CI"]);
    git(dir.path(), &["config", "user.email", "ci@example.test"]);
    fs::create_dir(dir.path().join("dir")).expect("directory");
    fs::write(dir.path().join("file"), b"first\n").expect("file");
    fs::write(dir.path().join("dir/stable"), b"unchanged\n").expect("stable");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "first"]);
    git(dir.path(), &["branch", "first"]);
    fs::write(dir.path().join("file"), b"second\n").expect("file");
    fs::write(dir.path().join("latest"), b"latest\n").expect("latest");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "second"]);
    git(dir.path(), &["switch", "-qc", "same-tree"]);
    git(dir.path(), &["commit", "--allow-empty", "-qm", "same tree"]);
    git(dir.path(), &["switch", "-q", "main"]);
    dir
}

fn no_checkout(source: &Path, with_grit: bool) -> tempfile::TempDir {
    let dest = tempfile::tempdir().expect("destination");
    if with_grit {
        clone_repo(
            CloneOptions {
                checkout: false,
                ..CloneOptions::new(source.to_string_lossy(), dest.path().to_owned())
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("grit clone");
    } else {
        git(
            source,
            &[
                "clone",
                "-q",
                "--no-checkout",
                ".",
                dest.path().to_str().unwrap(),
            ],
        );
    }
    assert!(!dest.path().join(".git/index").exists());
    git(dest.path(), &["branch", "first", "origin/first"]);
    git(dest.path(), &["branch", "same-tree", "origin/same-tree"]);
    dest
}

fn open(path: &Path, alternate_index: Option<&Path>) -> Repository {
    let mut environment = Environment::empty();
    environment.cwd = path.to_owned();
    environment.git_index_file = alternate_index.map(|p| p.to_string_lossy().into_owned());
    Repository::open(
        &path.join(".git"),
        &RepositoryOptions::with_environment(environment).with_work_tree(path),
    )
    .expect("repository")
}

fn oid(path: &Path, revision: &str) -> ObjectId {
    ObjectId::from_hex(git(path, &["rev-parse", revision]).trim()).expect("oid")
}

fn switch(
    repo: &Repository,
    mode: SwitchMode,
    dry_run: bool,
) -> grit_lib::error::Result<SwitchOutcome> {
    switch_repository(
        repo,
        &SwitchOptions {
            mode,
            dry_run,
            hooks: PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &NeverCancel,
    )
}

fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).expect("read directory") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).expect("bytes"),
                );
            }
        }
    }
    result
}

fn assert_materialized(path: &Path, expected: ObjectId) {
    assert_eq!(oid(path, "HEAD"), expected);
    assert!(path.join(".git/index").is_file());
    assert_eq!(
        git(path, &["write-tree"]).trim(),
        git(path, &["rev-parse", "HEAD^{tree}"]).trim()
    );
    assert!(git(path, &["status", "--porcelain"]).is_empty());
    git(path, &["fsck", "--strict"]);
}

#[test]
fn first_switch_materializes_entire_target_from_either_clone_implementation() {
    let source = source();
    for with_grit in [false, true] {
        for (args, revision) in [
            (vec!["--detach", "HEAD~1"], "HEAD~1"),
            (vec!["--detach", "HEAD"], "HEAD"),
            (vec!["main"], "main"),
            (vec!["first"], "first"),
            (vec!["same-tree"], "same-tree"),
            (vec!["-c", "new", "HEAD"], "HEAD"),
            (vec!["-c", "new", "HEAD~1"], "HEAD~1"),
            (vec!["-c", "new", "same-tree"], "same-tree"),
        ] {
            let dest = no_checkout(source.path(), with_grit);
            let oracle = no_checkout(source.path(), with_grit);
            let repo = open(dest.path(), None);
            let target = oid(dest.path(), revision);
            let mode = if args[0] == "--detach" {
                SwitchMode::Detached {
                    commit: target,
                    reflog_to_label: args[1].into(),
                }
            } else if args[0] == "-c" {
                SwitchMode::Create {
                    branch: "new".into(),
                    start_oid: target,
                    create_source_label: args[2].into(),
                    track_upstream: None,
                }
            } else {
                SwitchMode::Existing {
                    branch: args[0].into(),
                    oid: target,
                }
            };
            let before = bytes(dest.path());
            let preview = switch(&repo, mode.clone(), true).expect("initial preview");
            assert_eq!(
                preview.changed_paths,
                git(dest.path(), &["ls-tree", "-r", "--name-only", revision])
                    .lines()
                    .count()
            );
            assert!(preview.dry_run);
            assert_eq!(preview.commit, target);
            assert_eq!(
                bytes(dest.path()),
                before,
                "preview must preserve every repository byte"
            );
            let outcome = switch(&repo, mode, false).expect("initial switch");
            assert_eq!(outcome.commit, target);
            assert_eq!(outcome.created, args[0] == "-c");
            let mut git_args = vec!["switch", "-q"];
            git_args.extend(args.iter().copied());
            git(oracle.path(), &git_args);
            assert_materialized(dest.path(), target);
            assert_materialized(oracle.path(), target);
            assert_eq!(
                fs::read(dest.path().join("file")).unwrap(),
                fs::read(oracle.path().join("file")).unwrap()
            );
            assert_eq!(
                fs::read(dest.path().join("dir/stable")).unwrap(),
                b"unchanged\n"
            );
            assert_eq!(
                git(dest.path(), &["ls-files", "--stage"]),
                git(oracle.path(), &["ls-files", "--stage"])
            );
            assert_eq!(
                git(dest.path(), &["rev-parse", "--symbolic-full-name", "HEAD"]),
                git(
                    oracle.path(),
                    &["rev-parse", "--symbolic-full-name", "HEAD"]
                )
            );
            let log = fs::read_to_string(dest.path().join(".git/logs/HEAD")).expect("reflog");
            let previous = oid(source.path(), "main");
            assert!(log
                .lines()
                .last()
                .unwrap()
                .starts_with(&format!("{previous} {target} ")));
        }
    }
}

#[test]
fn already_detached_without_index_still_materializes() {
    let source = source();
    let dest = no_checkout(source.path(), false);
    let target = oid(dest.path(), "HEAD");
    git(
        dest.path(),
        &["update-ref", "--no-deref", "HEAD", &target.to_hex()],
    );
    let repo = open(dest.path(), None);
    let mode = SwitchMode::Detached {
        commit: target,
        reflog_to_label: "HEAD".into(),
    };
    assert_eq!(switch(&repo, mode.clone(), true).unwrap().changed_paths, 3);
    switch(&repo, mode, false).expect("same detached commit");
    assert_materialized(dest.path(), target);
}

#[test]
fn initial_switch_refuses_untracked_collisions_without_mutating_any_bytes() {
    let source = source();
    for with_grit in [false, true] {
        for path in ["file", "dir/stable"] {
            let dest = no_checkout(source.path(), with_grit);
            fs::create_dir_all(dest.path().join("dir")).unwrap();
            fs::write(dest.path().join(path), b"precious untracked contents\n").unwrap();
            let target = oid(dest.path(), "HEAD~1");
            let repo = open(dest.path(), None);
            // A failed create must not leave a branch behind.
            let mode = SwitchMode::Create {
                branch: "new".into(),
                start_oid: target,
                create_source_label: "HEAD~1".into(),
                track_upstream: None,
            };
            let before = bytes(dest.path());
            for dry_run in [true, false] {
                let err = switch(&repo, mode.clone(), dry_run).expect_err("untracked collision");
                match err {
                    Error::WouldOverwrite {
                        reason: OverwriteReason::Untracked,
                        paths,
                        ..
                    } => assert_eq!(paths, vec![path.to_owned()]),
                    other => panic!("expected untracked refusal: {other:?}"),
                }
                assert_eq!(bytes(dest.path()), before);
            }
            let out = Command::new("git")
                .current_dir(dest.path())
                .args(["switch", "-c", "new", "HEAD~1"])
                .output()
                .unwrap();
            assert!(!out.status.success());
            assert_eq!(bytes(dest.path()), before);
        }
    }
}

#[test]
fn existing_empty_index_preserves_staged_deletions() {
    let source = source();
    let dest = no_checkout(source.path(), false);
    git(dest.path(), &["read-tree", "--empty"]);
    let repo = open(dest.path(), None);
    let target = oid(dest.path(), "HEAD~1");
    let mode = SwitchMode::Detached {
        commit: target,
        reflog_to_label: "HEAD~1".into(),
    };
    let before = bytes(dest.path());
    for dry_run in [true, false] {
        assert!(matches!(
            switch(&repo, mode.clone(), dry_run),
            Err(Error::WouldOverwrite {
                reason: OverwriteReason::LocalChanges,
                ..
            })
        ));
        assert_eq!(bytes(dest.path()), before);
    }
    let out = Command::new("git")
        .current_dir(dest.path())
        .args(["switch", "--detach", "HEAD~1"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(bytes(dest.path()), before);

    // Moving to an equal tree must retain the staged deletions instead of materializing it.
    let target = oid(dest.path(), "same-tree");
    let mode = SwitchMode::Existing {
        branch: "same-tree".into(),
        oid: target,
    };
    assert_eq!(switch(&repo, mode.clone(), true).unwrap().changed_paths, 0);
    assert_eq!(bytes(dest.path()), before);
    switch(&repo, mode, false).expect("preserve staged deletions on same-tree switch");
    git(dest.path(), &["switch", "-q", "main"]);
    assert!(git(dest.path(), &["ls-files"]).is_empty());
    assert!(!dest.path().join("file").exists());
    assert!(!dest.path().join("dir/stable").exists());
    assert!(!dest.path().join("latest").exists());
    assert_eq!(
        git(dest.path(), &["status", "--porcelain"]),
        "D  dir/stable\nD  file\nD  latest\n"
    );
}

#[test]
fn initial_switch_uses_missing_effective_index_even_when_default_index_exists() {
    let source = source();
    let dest = no_checkout(source.path(), false);
    git(dest.path(), &["read-tree", "--empty"]);
    // Keep the empty tree referenced by the default index available to Git's fsck.
    git(dest.path(), &["mktree"]);
    let default_index = fs::read(dest.path().join(".git/index")).unwrap();
    let alternate = dest.path().join(".git/alternate-index");
    let repo = open(dest.path(), Some(&alternate));
    let target = oid(dest.path(), "HEAD~1");
    let mode = SwitchMode::Detached {
        commit: target,
        reflog_to_label: "HEAD~1".into(),
    };
    let before = bytes(dest.path());
    assert_eq!(switch(&repo, mode.clone(), true).unwrap().changed_paths, 2);
    assert_eq!(bytes(dest.path()), before);
    switch(&repo, mode, false).expect("materialize with effective index");
    assert_eq!(
        fs::read(dest.path().join(".git/index")).unwrap(),
        default_index
    );
    assert!(alternate.is_file());
    let output = Command::new("git")
        .current_dir(dest.path())
        .env("GIT_INDEX_FILE", &alternate)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    git(dest.path(), &["fsck", "--strict"]);
}

#[cfg(unix)]
#[test]
fn initial_switch_hook_receives_actual_previous_head() {
    use std::os::unix::fs::PermissionsExt;
    let source = source();
    let dest = no_checkout(source.path(), true);
    let previous = oid(dest.path(), "HEAD");
    let target = oid(dest.path(), "HEAD~1");
    let hook = dest.path().join(".git/hooks/post-checkout");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > .git/checkout-args\n",
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let repo = open(dest.path(), None);
    let mode = SwitchMode::Detached {
        commit: target,
        reflog_to_label: "HEAD~1".into(),
    };
    let before = bytes(dest.path());
    let mut opts = SwitchOptions {
        mode,
        dry_run: true,
        hooks: PostOperationHooks::Run,
    };
    switch_repository(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
    assert_eq!(bytes(dest.path()), before);
    opts.dry_run = false;
    let outcome = switch_repository(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
    assert_eq!(outcome.hooks_run, vec![grit_lib::HookName::PostCheckout]);
    assert_eq!(
        fs::read_to_string(dest.path().join(".git/checkout-args")).unwrap(),
        format!("{previous}\n{target}\n1\n")
    );
    assert_materialized(dest.path(), target);
}
