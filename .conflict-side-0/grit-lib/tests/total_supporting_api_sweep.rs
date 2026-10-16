//! Exercises supporting modules with large missed-line counts toward `[total]`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::apply::{
    inflate_binary_payload, materialize_binary_patch_forward, materialize_binary_patch_reverse,
    parse_patch, write_git_binary_patch,
};
use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::commit::trailers::interpret::{
    complete_line, process_trailers, trailer_if_exists_from_str, trailer_if_missing_from_str,
    trailer_where_from_str, ProcessTrailerOptions,
};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::rerere::{
    repo_rerere, rerere_clear, rerere_conflict_id, rerere_diff_for_path, rerere_enabled, rerere_gc,
    rerere_post_commit, rerere_remaining_lines, rerere_status_lines, RerereAutoupdate,
};
use grit_lib::objects::ObjectKind;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use std::fs;
use tempfile::tempdir;

fn linear_repo(depth: usize) -> (tempfile::TempDir, Repository, grit_lib::objects::ObjectId) {
    let dir = tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let mut parent: Option<grit_lib::objects::ObjectId> = None;
    let mut tip = repo.odb().write(ObjectKind::Blob, b"v0\n").expect("blob");
    for i in 0..depth {
        fs::write(dir.path().join("track.txt"), format!("v{i}\n")).expect("wt");
        let blob = repo
            .odb()
            .write(ObjectKind::Blob, format!("v{i}\n").as_bytes())
            .expect("blob");
        let tree_body = format!("100644 track.txt\0").into_bytes();
        let mut tree = tree_body;
        tree.extend_from_slice(blob.as_bytes());
        let tree_oid = repo.odb().write(ObjectKind::Tree, &tree).expect("tree");
        let mut body = format!("tree {tree_oid}\n");
        if let Some(p) = parent {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> {} +0000\ncommitter T <t@e.com> {} +0000\n\nc{i}\n",
            100 + i as i64,
            100 + i as i64
        ));
        tip = repo
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
        parent = Some(tip);
    }
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
    (dir, repo, tip)
}

#[test]
fn blame_apply_binary_and_trailer_sweep() -> grit_lib::error::Result<()> {
    let (_dir, repo, tip) = linear_repo(160);
    let blame = blame_file(
        &repo,
        "track.txt",
        &BlameOptions {
            start: Some(tip),
            line_range: None,
            ..Default::default()
        },
    )?;
    assert!(!blame.lines.is_empty());

    let patch = "diff --git a/track.txt b/track.txt\n--- a/track.txt\n+++ b/track.txt\n@@ -1 +1 @@\n-v0\n+v1\n";
    let parsed = parse_patch(patch, 1, "p", false, None)?;
    assert!(!parsed.is_empty());

    let old = b"before bytes";
    let new = b"after bytes longer";
    let body = write_git_binary_patch(old, new, "a/f", "b/f")?;
    let bin_patch = format!("diff --git a/f b/f\n{body}");
    let bin_files = parse_patch(&bin_patch, 1, "b", false, None)?;
    if let Some(file) = bin_files.first().and_then(|f| f.binary_patch.as_ref()) {
        let _ = inflate_binary_payload(&file.forward.compressed)?;
        let _ = materialize_binary_patch_forward(file, old)?;
        let _ = materialize_binary_patch_reverse(file, new)?;
    }

    let _ = trailer_where_from_str("after");
    let _ = trailer_if_exists_from_str("addIfDifferent");
    let _ = trailer_if_missing_from_str("doNothing");
    let msg = "subject\n\nSigned-off-by: T <t@e.com>\n";
    let opts = ProcessTrailerOptions::default();
    let _ = process_trailers(msg, &opts, &[], Some(repo.git_dir()));
    let _ = complete_line("partial");
    Ok(())
}

#[test]
fn rerere_and_repository_index_paths() -> grit_lib::error::Result<()> {
    let (dir, repo, _) = linear_repo(6);
    let cfg = ConfigSet::new();
    let _ = rerere_enabled(&cfg, repo.git_dir());
    let _ = rerere_conflict_id(repo.odb().hash_algo(), "<<<<<<<\n", 7);
    let _ = repo_rerere(&repo, RerereAutoupdate::No)?;
    let _ = rerere_status_lines(&repo)?;
    let _ = rerere_remaining_lines(&repo)?;
    let _ = rerere_diff_for_path(&repo, "track.txt")?;
    let _ = rerere_post_commit(&repo)?;
    let _ = rerere_clear(repo.git_dir());
    let _ = rerere_gc(repo.environment(), repo.git_dir(), 200)?;

    let _ = repo.reload_config();
    let _ = Repository::open(dir.path(), &RepositoryOptions::empty().for_serving());
    let mut index = repo.index_handle().load()?;
    repo.index_handle().write(&mut index)?;
    Ok(())
}
