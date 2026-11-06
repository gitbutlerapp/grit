//! Batched stat diffs over packed history written by grit and system Git.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{CommitFilesManyOptions, DiffDetail, DiffOptions, RenameDetection};
use grit_lib::objects::{
    serialize_commit, serialize_tree, CommitData, ObjectId, ObjectKind, TreeEntry,
};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};
use grit_test_support::git;

#[test]
fn packed_batch_stats_match_git_for_small_edits_and_file_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "-q"]);
    git(path, &["config", "gc.auto", "0"]);
    let open = || {
        Repository::open(
            &path.join(".git"),
            &RepositoryOptions::empty().with_work_tree(path),
        )
        .unwrap()
    };
    let repo = open();
    let prefix = "common prefix\n".repeat(300);
    let suffix = "common suffix\n".repeat(300);
    let mut commits = Vec::<ObjectId>::new();
    let mut last_tree = None;
    for i in 0..36 {
        let text = format!(
            "{prefix}edit {i}\n{suffix}{}",
            if i % 2 == 0 { "EOF" } else { "EOF\n" }
        );
        let blob = repo.odb().write(ObjectKind::Blob, text.as_bytes()).unwrap();
        let binary = repo.odb().write(ObjectKind::Blob, &[0, i as u8]).unwrap();
        let mut invalid_text = b"invalid UTF-8: \xff\n".to_vec();
        invalid_text.extend_from_slice(format!("edit {i}\n").as_bytes());
        let invalid = repo.odb().write(ObjectKind::Blob, &invalid_text).unwrap();
        let mut entries = vec![
            TreeEntry {
                mode: 0o100644,
                name: b"binary".to_vec(),
                oid: binary,
            },
            TreeEntry {
                mode: if i % 3 == 0 { 0o100755 } else { 0o100644 },
                name: b"file".to_vec(),
                oid: blob,
            },
            TreeEntry {
                mode: 0o100644,
                name: b"invalid".to_vec(),
                oid: invalid,
            },
        ];
        if i % 2 == 0 {
            entries.push(TreeEntry {
                mode: 0o100644,
                name: b"extra".to_vec(),
                oid: blob,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let tree = repo
            .odb()
            .write(ObjectKind::Tree, &serialize_tree(&entries))
            .unwrap();
        let identity = "Stat Test <stat@example.com> 1700000000 +0000";
        let data = CommitData {
            tree,
            parents: commits.last().copied().into_iter().collect(),
            author: identity.into(),
            committer: identity.into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("edit {i}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        commits.push(
            repo.odb()
                .write(ObjectKind::Commit, &serialize_commit(&data))
                .unwrap(),
        );
        last_tree = Some(tree);
    }
    let tip = commits.last().unwrap().to_hex();
    let tree = last_tree.unwrap().to_hex();
    git(path, &["update-ref", "HEAD", &tip]);
    git(path, &["fsck", "--strict"]);
    // Add a Git-written commit, then make every read go through packs and deltas.
    let git_commit = git(
        path,
        &["commit-tree", &tree, "-p", &tip, "-m", "git commit"],
    );
    let git_commit: ObjectId = git_commit.trim().parse().unwrap();
    git(path, &["update-ref", "HEAD", &git_commit.to_hex()]);
    commits.push(git_commit);
    git(path, &["repack", "-adf"]);
    git(path, &["prune-packed"]);
    drop(repo);
    let repo = open();
    commits.reverse();
    let config = ConfigSet::new();
    let mut diff_options = DiffOptions::default();
    diff_options.renames = RenameDetection::Off;
    let options = CommitFilesManyOptions {
        diff_options,
        detail: DiffDetail::Stat,
        skip_merge_patches: true,
    };
    let empty_ps = Pathspec::entire_tree(PathspecGlobalFlags::default());
    let parallel = repo
        .diff()
        .commit_files_many(
            &commits,
            &empty_ps,
            options.clone(),
            &config,
            &ThreadPolicy::with_threads_override(Some(4)),
        )
        .unwrap();
    let serial = repo
        .diff()
        .commit_files_many(
            &commits,
            &empty_ps,
            options.clone(),
            &config,
            &ThreadPolicy::with_threads_override(Some(1)),
        )
        .unwrap();
    assert_eq!(parallel, serial);
    for (oid, result) in commits.iter().zip(&parallel) {
        let actual = result
            .output
            .files
            .iter()
            .map(|file| {
                assert!(
                    file.structured.is_none(),
                    "stat must not retain patch hunks"
                );
                match file.stat {
                    Some(stat) => format!("{}\t{}\t{}", stat.insertions, stat.deletions, file.path),
                    None => format!("-\t-\t{}", file.path),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let expected = git(
            path,
            &[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "-r",
                "--no-renames",
                "--numstat",
                &oid.to_hex(),
            ],
        );
        assert_eq!(actual, expected.trim_end(), "commit {oid}");
    }
    let file_ps = Pathspec::scoped(["file"], PathspecGlobalFlags::default());
    let filtered = repo
        .diff()
        .commit_files_many(
            &commits,
            &file_ps,
            options,
            &config,
            &ThreadPolicy::with_threads_override(Some(4)),
        )
        .unwrap();
    for (all, filtered) in parallel.iter().zip(&filtered) {
        let expected: Vec<_> = all
            .output
            .files
            .iter()
            .filter(|file| file.path == "file")
            .cloned()
            .collect();
        assert_eq!(filtered.output.files, expected);
    }
    git(path, &["fsck", "--strict"]);
}

/// Sum `git diff-tree --numstat` for one commit; `None` when Git prints nothing (merges).
fn git_line_totals(
    path: &std::path::Path,
    oid: &ObjectId,
    paths: &[&str],
) -> Option<(usize, usize, usize)> {
    let mut args = vec![
        "diff-tree",
        "--root",
        "--no-commit-id",
        "-r",
        "-M",
        "--numstat",
    ];
    let hex = oid.to_hex();
    args.push(&hex);
    if !paths.is_empty() {
        args.push("--");
        args.extend_from_slice(paths);
    }
    let out = git(path, &args);
    if out.trim().is_empty() {
        return None;
    }
    Some(out.lines().fold((0, 0, 0), |(files, ins, del), line| {
        let mut cols = line.split('\t');
        let count = |c: Option<&str>| c.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
        let added = count(cols.next());
        let removed = count(cols.next());
        (files + 1, ins + added, del + removed)
    }))
}

#[test]
fn commit_line_totals_match_git_numstat_and_skip_merges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("gc.auto", "0"),
        ("user.name", "Stat Test"),
        ("user.email", "stat@example.com"),
        ("commit.gpgsign", "false"),
    ] {
        git(path, &["config", key, value]);
    }
    let commit = |msg: &str| git(path, &["commit", "-q", "-m", msg]);
    let body = (0..40).map(|i| format!("line {i}\n")).collect::<String>();
    std::fs::write(path.join("a.txt"), &body).unwrap();
    std::fs::write(path.join("b.txt"), "b\n").unwrap();
    git(path, &["add", "."]);
    commit("root");
    git(path, &["mv", "a.txt", "moved.txt"]);
    std::fs::write(path.join("moved.txt"), format!("{body}tail\n")).unwrap();
    git(path, &["add", "."]);
    commit("rename with an edit");
    git(path, &["switch", "-q", "-c", "side", "HEAD~1"]);
    std::fs::write(path.join("b.txt"), "b\nside\n").unwrap();
    std::fs::write(path.join("bin.dat"), [0u8, 1, 2]).unwrap();
    git(path, &["add", "."]);
    commit("side edit with a binary file");
    git(path, &["switch", "-q", "main"]);
    git(path, &["merge", "-q", "--no-edit", "side"]);
    std::fs::write(path.join("b.txt"), "replaced\n").unwrap();
    git(path, &["add", "."]);
    commit("after merge");
    git(path, &["repack", "-adq"]);
    git(path, &["fsck", "--strict"]);

    let repo = Repository::open(
        &path.join(".git"),
        &RepositoryOptions::empty().with_work_tree(path),
    )
    .unwrap();
    let oids: Vec<ObjectId> = git(path, &["rev-list", "HEAD"])
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    assert_eq!(oids.len(), 5);
    let config = ConfigSet::new();
    for (paths, specs) in [(vec![], vec![]), (vec!["b.txt"], vec!["b.txt".to_owned()])] {
        for threads in [1, 4] {
            let pathspec = if specs.is_empty() {
                Pathspec::entire_tree(PathspecGlobalFlags::default())
            } else {
                Pathspec::scoped(specs.clone(), PathspecGlobalFlags::default())
            };
            let totals = repo
                .diff()
                .commit_line_totals(
                    &oids,
                    &pathspec,
                    DiffOptions::default(),
                    &config,
                    &ThreadPolicy::with_threads_override(Some(threads)),
                )
                .unwrap();
            for (oid, totals) in oids.iter().zip(totals) {
                let ours = totals.map(|t| (t.files_changed, t.insertions, t.deletions));
                let theirs = git_line_totals(path, oid, &paths);
                // Git prints nothing for a commit whose pathspec matches no change.
                let theirs = theirs.or_else(|| {
                    (git(path, &["rev-list", "--parents", "-n1", &oid.to_hex()])
                        .split_whitespace()
                        .count()
                        <= 2)
                        .then_some((0, 0, 0))
                });
                assert_eq!(ours, theirs, "commit {oid} paths {paths:?}");
            }
        }
    }
}
