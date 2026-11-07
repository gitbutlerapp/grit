//! Pruning a custom primary keeps exactly what the system `git prune` keeps on the same
//! history in a files repository, and the files repository stays valid for `git fsck`.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use grit_lib::maintenance::PruneOptions;
use grit_lib::objects::ObjectId;

use crate::prune_custom_store::{
    build_history, fixed_now, init, memory_repo, open_files, stored, PRUNE_NOW,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}

/// The first column of every output line, as object ids.
fn oids_in(output: &str) -> BTreeSet<ObjectId> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|hex| ObjectId::from_hex(hex).expect("oid"))
        .collect()
}

fn all_objects(dir: &Path) -> BTreeSet<ObjectId> {
    oids_in(&git(
        dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    ))
}

#[test]
fn memory_primary_keeps_what_git_prune_keeps() {
    let files_dir = tempfile::tempdir().expect("tempdir");
    init(files_dir.path(), PRUNE_NOW);
    let files = open_files(files_dir.path());
    let files_history = build_history(&files, files_dir.path());

    let (memory_dir, memory, store) = memory_repo(PRUNE_NOW);
    build_history(&memory, memory_dir.path());

    let files_before = all_objects(files_dir.path());
    assert_eq!(
        files_before,
        stored(store.as_ref()),
        "same history, same objects"
    );
    let git_would_prune = oids_in(&git(files_dir.path(), &["prune", "-n", "--expire=now"]));
    assert_eq!(
        git_would_prune,
        files_history.unreachable.iter().copied().collect(),
        "git agrees on what is unreachable"
    );

    let opts = PruneOptions::default().with_now(fixed_now());
    files
        .maintenance()
        .prune_loose_objects(&opts)
        .expect("files prune");
    memory
        .maintenance()
        .prune_loose_objects(&opts)
        .expect("memory prune");

    let survivors = stored(store.as_ref());
    assert_eq!(
        files_before
            .difference(&survivors)
            .copied()
            .collect::<BTreeSet<_>>(),
        git_would_prune,
        "the memory primary lost exactly what git prune drops"
    );
    assert_eq!(all_objects(files_dir.path()), survivors);
    assert!(git(files_dir.path(), &["prune", "-n", "--expire=now"]).is_empty());
    git(files_dir.path(), &["fsck", "--full", "--strict"]);
}
