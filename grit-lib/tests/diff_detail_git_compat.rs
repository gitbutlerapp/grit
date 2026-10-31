//! `--stat` / `--name-only` / `--name-status` compatibility with system `git`.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use grit_lib::diff::{
    build_file_diffs, diff_trees_with_renames_and_policy, DiffDetail, DiffOptions, DiffStatus,
    DiffTarget, ListedFileDiff, RenameDetection,
};
use grit_lib::objects::{ObjectKind, TreeEntry};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};

fn init_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

fn write_blob(repo: &Repository, content: &[u8]) -> grit_lib::objects::ObjectId {
    repo.odb()
        .write(ObjectKind::Blob, content)
        .expect("write blob")
}

fn tree_with(repo: &Repository, entries: &[(&str, &[u8])]) -> grit_lib::objects::ObjectId {
    let mut tes = Vec::new();
    for (name, data) in entries {
        let oid = write_blob(repo, data);
        tes.push(TreeEntry {
            mode: 0o100644,
            name: name.as_bytes().to_vec(),
            oid,
        });
    }
    let data = grit_lib::objects::serialize_tree(&tes);
    repo.odb()
        .write(ObjectKind::Tree, &data)
        .expect("write tree")
}

fn grit_numstat_for_trees(
    repo: &Repository,
    old: Option<&grit_lib::objects::ObjectId>,
    new: Option<&grit_lib::objects::ObjectId>,
) -> Vec<(usize, usize, String)> {
    let entries = repo.diff().tree_to_tree(old, new).expect("tree diff");
    let config = repo.config().expect("config");
    let out = build_file_diffs(
        repo.odb(),
        entries,
        DiffDetail::Stat,
        DiffOptions::default(),
        config.as_ref(),
        &ThreadPolicy::default(),
    )
    .expect("stat diff");
    out.files
        .into_iter()
        .map(|f| {
            let stat = f.stat.unwrap_or_default();
            (stat.insertions, stat.deletions, f.path)
        })
        .collect()
}

#[test]
fn diff_stat_counts_match_git_numstat_trees_text_and_binary() {
    let (_dir, repo) = init_repo();
    let old = tree_with(&repo, &[("a.txt", b"one\n")]);
    let new = tree_with(
        &repo,
        &[("a.txt", b"one\ntwo\n"), ("bin.dat", &[0, 1, 2, 3])],
    );
    let grit = grit_numstat_for_trees(&repo, Some(&old), Some(&new));
    assert!(grit.iter().any(|(_, _, p)| p == "a.txt"));
    assert!(grit
        .iter()
        .any(|(i, d, p)| p == "a.txt" && *i == 1 && *d == 0));
    assert!(grit
        .iter()
        .any(|(i, d, p)| p == "bin.dat" && *i == 0 && *d == 0));
}

#[test]
fn tree_diff_stat_matches_structured_patch_repeated_anchor() {
    use grit_lib::diff::structured::{structured_file_diff, SideBytes};
    use grit_lib::diff::LineStat;
    use grit_lib::objects::FileMode;

    let anchor = "fn anchor() {\n";
    let old_body = format!("{anchor}{anchor}// old body\n");
    let mut new_body = String::new();
    for i in 0..800 {
        new_body.push_str(&format!("// inserted block {i}\n"));
    }
    new_body.push_str(&anchor);
    new_body.push_str("// new tail\n");

    let (_dir, repo) = init_repo();
    let old = tree_with(&repo, &[("feature.rs", old_body.as_bytes())]);
    let new = tree_with(&repo, &[("feature.rs", new_body.as_bytes())]);
    let grit = grit_numstat_for_trees(&repo, Some(&old), Some(&new));
    let (_, _, path) = grit.first().expect("one file");
    assert_eq!(path, "feature.rs");
    let (tree_ins, tree_del, _) = grit[0];

    let structured = structured_file_diff(
        FileMode::REGULAR,
        FileMode::REGULAR,
        &SideBytes::from_bytes(old_body.into_bytes()),
        &SideBytes::from_bytes(new_body.into_bytes()),
        DiffOptions::default(),
    );
    let patch = LineStat::from_hunks(&structured.hunks);
    assert_eq!(
        (tree_ins, tree_del),
        (patch.insertions, patch.deletions),
        "tree --stat must match patch line counts (issue #2236)"
    );
}

#[test]
fn identical_binary_rename_keeps_binary_flag_in_stat_mode() {
    let (_dir, repo) = init_repo();
    let old = tree_with(&repo, &[("f.txt", b"alpha\n")]);
    let new = tree_with(&repo, &[("f.txt", b"beta\n")]);
    let entries = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new))
        .expect("entries");
    let counter = Arc::new(AtomicUsize::new(0));
    let config = repo.config().expect("config");
    let mut opts = DiffOptions::default();
    opts.blob_read_counter = Some(Arc::clone(&counter));
    let _ = build_file_diffs(
        repo.odb(),
        entries,
        DiffDetail::NameOnly,
        opts,
        config.as_ref(),
        &ThreadPolicy::default(),
    )
    .expect("name-only");
    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    let new_tree = tree_with(&repo, &[("f", b"x\n")]);
    let _target = DiffTarget::Trees {
        old: None,
        new: new_tree,
    };
}

fn metadata_tree(repo: &Repository, mut entries: Vec<TreeEntry>) -> grit_lib::objects::ObjectId {
    entries.sort_by(|a, b| {
        grit_lib::objects::tree_entry_cmp(&a.name, a.mode == 0o040000, &b.name, b.mode == 0o040000)
    });
    repo.odb()
        .write(
            ObjectKind::Tree,
            &grit_lib::objects::serialize_tree(&entries),
        )
        .expect("tree")
}

fn git_metadata(repo: &Repository, args: &[&str]) -> Vec<u8> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo.git_dir())
        .args(args)
        .output()
        .expect("system git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn tree_metadata_matrix_matches_git_without_reading_blobs() {
    use grit_lib::objects::{HashAlgo, ObjectId};
    use std::sync::atomic::Ordering;

    for algo in [HashAlgo::Sha1, HashAlgo::Sha256] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default().with_object_format(algo))
            .expect("init");
        let a = write_blob(&repo, b"before\n");
        let b = write_blob(&repo, b"after\n");
        let entry = |name: &[u8], mode, oid: ObjectId| TreeEntry {
            name: name.to_vec(),
            mode,
            oid,
        };
        let child_a = metadata_tree(&repo, vec![entry(b"leaf", 0o100644, a)]);
        let child_b = metadata_tree(&repo, vec![entry(b"leaf", 0o100644, b)]);
        let old = metadata_tree(
            &repo,
            vec![
                entry(b"deleted", 0o100644, a),
                entry(b"exec", 0o100644, a),
                entry(b"left", 0o040000, child_a),
                entry(b"node", 0o100644, a),
                entry(b"raw-\xff", 0o100644, a),
                entry(b"right", 0o040000, child_a),
                entry(b"type", 0o100644, a),
            ],
        );
        let new = metadata_tree(
            &repo,
            vec![
                entry(b"added", 0o100644, b),
                entry(b"exec", 0o100755, a),
                entry(b"left", 0o040000, child_b),
                entry(b"node", 0o040000, child_b),
                entry(b"raw-\xff", 0o100644, b),
                entry(b"right", 0o040000, child_b),
                entry(b"type", 0o120000, b),
            ],
        );
        let empty = metadata_tree(&repo, vec![]);
        git_metadata(&repo, &["fsck", "--strict", &old.to_hex(), &new.to_hex()]);
        // Have Git write the index/tree too, then read that tree through the API.
        git_metadata(&repo, &["read-tree", &new.to_hex()]);
        let git_tree = String::from_utf8(git_metadata(&repo, &["write-tree"]))
            .expect("oid")
            .trim()
            .parse::<ObjectId>()
            .expect("git-written tree");
        assert_eq!(git_tree, new);

        let cases = [
            (Some(old), Some(git_tree)),
            (Some(new), Some(old)),
            (None, Some(new)),
            (Some(old), None),
            (Some(old), Some(old)),
            (Some(empty), Some(empty)),
            (None, None),
        ];
        let expected: Vec<_> = cases
            .iter()
            .map(|(old, new)| {
                git_metadata(
                    &repo,
                    &[
                        "diff",
                        "--no-renames",
                        "--name-status",
                        "-z",
                        &old.unwrap_or(empty).to_hex(),
                        &new.unwrap_or(empty).to_hex(),
                    ],
                )
            })
            .collect();
        let expected_trees = git_metadata(
            &repo,
            &[
                "diff-tree",
                "-r",
                "-t",
                "--no-commit-id",
                "--no-renames",
                "--name-status",
                "-z",
                &old.to_hex(),
                &new.to_hex(),
            ],
        );
        // Missing blobs make any accidental content read fail, including reads
        // outside the optional instrumentation in DiffOptions.
        std::fs::remove_file(repo.odb().object_path(&a)).expect("remove old blob");
        std::fs::remove_file(repo.odb().object_path(&b)).expect("remove new blob");
        for ((old, new), expected) in cases.into_iter().zip(expected) {
            let entries = repo
                .diff()
                .tree_to_tree(old.as_ref(), new.as_ref())
                .expect("metadata diff");
            for detail in [DiffDetail::NameOnly, DiffDetail::NameStatus] {
                let counter = Arc::new(AtomicUsize::new(0));
                let mut opts = DiffOptions::default().with_renames(RenameDetection::Off);
                opts.blob_read_counter = Some(Arc::clone(&counter));
                let out = build_file_diffs(
                    repo.odb(),
                    entries.clone(),
                    detail,
                    opts,
                    &grit_lib::config::ConfigSet::new(),
                    &ThreadPolicy::default(),
                )
                .expect("names");
                let actual: Vec<u8> = out
                    .files
                    .iter()
                    .flat_map(|file| {
                        assert!(file.stat.is_none() && file.structured.is_none());
                        let mut row = vec![file.status.letter() as u8, 0];
                        row.extend_from_slice(&grit_lib::repo_relpath::storage_bytes(&file.path));
                        row.push(0);
                        row
                    })
                    .collect();
                assert_eq!(actual, expected, "{algo:?} {detail:?}");
                assert_eq!(counter.load(Ordering::Relaxed), 0);
                let rows: Vec<_> = entries
                    .iter()
                    .cloned()
                    .map(ListedFileDiff::from_metadata)
                    .collect();
                assert_eq!(rows, out.files, "{algo:?} {detail:?} per-entry rows");
                for (entry, file) in entries.iter().zip(&out.files) {
                    assert_eq!(file.old_mode, entry.old_mode.octal_if_present());
                    assert_eq!(file.new_mode, entry.new_mode.octal_if_present());
                }
            }
            // With nothing to pair (only one side has adds or deletes), the rename pass the
            // CLI runs must keep the metadata list as is and still read no blob.
            let adds_and_deletes = entries.iter().any(|e| e.status == DiffStatus::Added)
                && entries.iter().any(|e| e.status == DiffStatus::Deleted);
            if !adds_and_deletes {
                let renamed = diff_trees_with_renames_and_policy(
                    repo.odb(),
                    old.as_ref(),
                    new.as_ref(),
                    "",
                    50,
                    &grit_lib::config::ConfigSet::new(),
                    &ThreadPolicy::default(),
                )
                .expect("rename pass without blobs");
                assert_eq!(renamed, entries, "{algo:?} rename pass");
            }
        }
        let shown =
            grit_lib::diff::diff_trees_show_tree_entries(repo.odb(), Some(&old), Some(&new), "")
                .expect("tree entries without blobs");
        let shown: Vec<u8> = shown
            .iter()
            .flat_map(|entry| {
                let mut row = vec![entry.status.letter() as u8, 0];
                row.extend_from_slice(&grit_lib::repo_relpath::storage_bytes(entry.path()));
                row.push(0);
                row
            })
            .collect();
        assert_eq!(shown, expected_trees, "show tree entries {algo:?}");
        let malformed = repo
            .odb()
            .write(ObjectKind::Tree, b"100644 truncated\0")
            .expect("malformed tree");
        assert!(matches!(
            repo.diff().tree_to_tree(None, Some(&malformed)),
            Err(grit_lib::error::Error::CorruptObject(_))
        ));
        // Identical-tree pruning does not attempt to parse even an unreadable tree.
        assert!(repo
            .diff()
            .tree_to_tree(Some(&malformed), Some(&malformed))
            .expect("equal OIDs")
            .is_empty());
    }
}
