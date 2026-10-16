//! Deep blame, CRLF, apply, and pathspec sweeps for `[total]` line coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::apply::parse_patch;
use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::crlf::{
    convert_to_git, convert_to_worktree_eager, crlf_to_lf, gather_convert_stats_ascii, has_crlf,
    has_crlf_in_index_blob, has_lone_lf, is_all_crlf, is_all_lf, is_binary, lf_to_crlf,
    load_gitattributes, ConversionConfig, FileAttrs,
};
use grit_lib::worktree::pathspec::{
    matches_pathspec_list, parse_pathspecs_from_source, pathspec_matches,
};
use std::fs;
use tempfile::tempdir;

fn seed_blame_repo(depth: usize) -> (tempfile::TempDir, Repository, grit_lib::objects::ObjectId) {
    let dir = tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let mut parent: Option<grit_lib::objects::ObjectId> = None;
    let mut tip = repo.odb().write(ObjectKind::Blob, b"l0\n").expect("blob");
    for i in 0..depth {
        let content = format!("line {i}\nsecond {i}\nthird {i}\n");
        fs::write(dir.path().join("file.txt"), &content).expect("wt");
        let blob = repo
            .odb()
            .write(ObjectKind::Blob, content.as_bytes())
            .expect("blob");
        let tree = format!("100644 file.txt\0").into_bytes();
        let mut tree_body = tree;
        tree_body.extend_from_slice(blob.as_bytes());
        let tree_oid = repo
            .odb()
            .write(ObjectKind::Tree, &tree_body)
            .expect("tree");
        let mut body = format!("tree {tree_oid}\n");
        if let Some(p) = parent {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> {} +0000\ncommitter T <t@e.com> {} +0000\n\nc{i}\n",
            200 + i as i64,
            200 + i as i64
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
fn blame_crlf_apply_and_pathspec_deep_sweep() -> grit_lib::error::Result<()> {
    let (_dir, repo, tip) = seed_blame_repo(1100);
    for start in [None, Some(tip)] {
        for range in [
            None,
            Some(1usize..=2usize),
            Some(2usize..=3usize),
            Some(1usize..=1usize),
        ] {
            let blame = blame_file(
                &repo,
                "file.txt",
                &BlameOptions {
                    start,
                    line_range: range,
                    ..Default::default()
                },
            )?;
            assert!(!blame.lines.is_empty());
        }
    }

    let mut cfg_blob = ConfigSet::new();
    fs::write(
        repo.git_dir().join("config"),
        "[core]\n\tautocrlf = input\n\tsafecrlf = warn\n",
    )
    .map_err(grit_lib::error::Error::Io)?;
    repo.reload_config()?;
    cfg_blob = (*repo.config()?).clone();
    let crlf = ConversionConfig::from_config(&cfg_blob);
    let attrs = FileAttrs::default();
    let samples: &[&[u8]] = &[b"a\r\nb\r\n", b"a\nb\n", b"\x00bin", b"all\nlf\n"];
    for sample in samples {
        let _ = convert_to_git(sample, "x.txt", &crlf, &attrs)?;
        let git = convert_to_git(sample, "x.txt", &crlf, &attrs)?;
        let _ = convert_to_worktree_eager(&git, "x.txt", &crlf, &attrs, None, None, None)?;
        let _ = crlf_to_lf(sample);
        let _ = lf_to_crlf(sample);
        let _ = gather_convert_stats_ascii(sample);
        let _ = has_crlf(sample);
        let _ = has_lone_lf(sample);
        let _ = is_all_crlf(sample);
        let _ = is_all_lf(sample);
        let _ = is_binary(sample);
    }
    let _ = has_crlf_in_index_blob(b"a\r\n");
    if let Some(wt) = repo.work_tree() {
        let _ = load_gitattributes(wt);
    }

    let patch = (0..40)
        .map(|i| {
            format!(
                "diff --git a/f{i}.txt b/f{i}.txt\n--- a/f{i}.txt\n+++ b/f{i}.txt\n@@ -1 +1 @@\n-old\n+new{i}\n"
            )
        })
        .collect::<String>();
    let files = parse_patch(&patch, 1, "p", false, None)?;
    assert!(files.len() >= 10);

    let specs = parse_pathspecs_from_source(b"*.txt\0:(exclude)skip/\0dir/\0", true)?;
    for path in ["f0.txt", "dir/x.txt", "skip/y.txt", "other.md"] {
        let _ = matches_pathspec_list(path, &specs);
        let _ = pathspec_matches("*.txt", path);
    }

    Ok(())
}
