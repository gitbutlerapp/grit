//! `--stat` / `--name-only` / `--name-status` compatibility with system `git`.

use grit_lib::diff::{
    build_file_diffs, diff_trees_with_renames_and_policy, DiffDetail, DiffOptions, DiffStatus,
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
fn identical_binary_rename_keeps_binary_flag_in_stat_mode() {
    let (_dir, repo) = init_repo();
    let bin: Vec<u8> = (0u8..128).collect();
    let old = tree_with(&repo, &[("data.bin", &bin)]);
    let new = tree_with(&repo, &[("renamed.bin", &bin)]);
    let config = repo.config().expect("config");
    let entries = diff_trees_with_renames_and_policy(
        repo.odb(),
        Some(&old),
        Some(&new),
        "",
        50,
        config.as_ref(),
        &ThreadPolicy::default(),
    )
    .expect("tree diff with renames");
    assert!(
        entries.iter().any(|e| e.status == DiffStatus::Renamed),
        "{entries:?}"
    );
    let out = build_file_diffs(
        repo.odb(),
        entries,
        DiffDetail::Stat,
        DiffOptions::default(),
        config.as_ref(),
        &ThreadPolicy::default(),
    )
    .expect("stat diff");
    assert_eq!(out.files.len(), 1, "{:?}", out.files);
    assert!(
        out.files[0].binary,
        "identical binary rename must stay binary"
    );
    assert!(out.files[0].stat.is_none());
}
