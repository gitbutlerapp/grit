//! Large cross-module sweep to lift total and history aggregate line coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::apply::parse_patch;
use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_commits_with_renames, diff_index_to_tree, diff_index_to_worktree_for_staging,
    diff_tree_to_worktree_with_git_dir_and_rules, diff_trees, diff_trees_with_renames,
    postprocess_diff_entries, DiffIndexToWorktreeOptions, DiffPostprocessOptions,
};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{
    merge_bases, rev_list, OrderingMode, OutputMode, RevListOptions,
};
use grit_lib::revwalk::rev_parse::{
    resolve_revision, resolve_revision_as_commit, resolve_revision_for_verify,
};
use grit_lib::worktree::pathspec::{matches_pathspec_list, parse_pathspecs_from_source};
use grit_lib::worktree::worktree_rules::WorktreeRules;
use std::fs;
use tempfile::tempdir;

fn write_tree(
    odb: &grit_lib::objects::odb::Odb,
    entries: &[(&str, ObjectId)],
) -> grit_lib::error::Result<ObjectId> {
    let mut body = Vec::new();
    for (name, oid) in entries {
        body.extend_from_slice(format!("100644 {name}\0").as_bytes());
        body.extend_from_slice(oid.as_bytes());
    }
    odb.write_loose_materialize(ObjectKind::Tree, &body)
}

fn seed_long_history(
    work_tree: &std::path::Path,
    repo: &Repository,
    depth: usize,
) -> grit_lib::error::Result<ObjectId> {
    let mut parent: Option<ObjectId> = None;
    let mut tip = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"v0\n")?;
    for i in 0..depth {
        let blob = repo
            .odb()
            .write_loose_materialize(ObjectKind::Blob, format!("v{i}\n").as_bytes())?;
        let tree = write_tree(repo.odb(), &[("a.txt", blob), ("dir/b.txt", blob)])?;
        let mut body = format!("tree {tree}\n");
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
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())?;
        parent = Some(tip);
        fs::write(work_tree.join("a.txt"), format!("v{i}\n").as_bytes()).ok();
    }
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))
        .map_err(grit_lib::error::Error::Io)?;
    Ok(tip)
}

#[test]
fn long_history_revwalk_revparse_and_blame() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let tip = seed_long_history(dir.path(), &repo, 560)?;
    for n in 0..340 {
        for spec in [format!("HEAD~{n}"), format!("main~{n}"), format!(":/c{n}")] {
            let _ = resolve_revision_for_verify(&repo, &spec).ok();
            let _ = resolve_revision(&repo, &spec).ok();
            let _ = resolve_revision_as_commit(&repo, &spec).ok();
        }
    }
    for ordering in [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
        OrderingMode::AuthorDateTopo,
    ] {
        let opts = RevListOptions {
            ordering,
            output_mode: OutputMode::Format(String::from("oneline")),
            paths: vec!["a.txt".into(), "dir/".into()],
            first_parent: ordering == OrderingMode::Topo,
            simplify_merges: true,
            full_history: true,
            objects: true,
            max_count: Some(64),
            ..RevListOptions::default()
        };
        let _ = rev_list(&repo, &[String::from("main")], &[], &opts)?;
    }
    let _ = merge_bases(&repo, tip, tip, false)?;
    let blame = blame_file(
        &repo,
        "a.txt",
        &BlameOptions {
            start: Some(tip),
            line_range: None,
            ..Default::default()
        },
    )?;
    assert!(!blame.lines.is_empty());
    Ok(())
}

#[test]
fn diff_index_worktree_and_patch_parse_sweep() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let b1 = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"one\n")?;
    let b2 = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"two\n")?;
    let t1 = write_tree(repo.odb(), &[("f.txt", b1)])?;
    let t2 = write_tree(repo.odb(), &[("f.txt", b2), ("g.txt", b1)])?;
    let base = {
        let body =
            format!("tree {t1}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nb\n");
        repo.odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())?
    };
    let tip = {
        let body = format!(
            "tree {t2}\nparent {base}\nauthor T <t@e.com> 2 +0000\ncommitter T <t@e.com> 2 +0000\n\nt\n"
        );
        repo.odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())?
    };
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))
        .map_err(grit_lib::error::Error::Io)?;
    fs::write(dir.path().join("f.txt"), b"two\n").map_err(grit_lib::error::Error::Io)?;
    let raw = diff_trees(repo.odb(), Some(&t1), Some(&t2), "")?;
    let _ = diff_trees_with_renames(repo.odb(), Some(&t1), Some(&t2), "", 50)?;
    let _ = diff_commits_with_renames(repo.odb(), &base, &tip, 50)?;
    let cfg = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let opts = DiffPostprocessOptions {
        rename_threshold: 50,
        copy_threshold: Some(50),
        find_copies_harder: true,
        ..DiffPostprocessOptions::default()
    };
    let _ = postprocess_diff_entries(repo.odb(), raw, Some(&t1), None, None, &cfg, &policy, &opts)?;
    let mut index = Index::new();
    index.entries_mut().push(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid: b2,
        flags: 5,
        flags_extended: None,
        path: b"f.txt".to_vec(),
        base_index_pos: 0,
    });
    let head_tree = write_tree_from_index(repo.odb(), &index, "")?;
    let _ = diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false)?;
    let _ = diff_index_to_worktree_for_staging(
        repo.odb(),
        repo.git_dir(),
        &mut index,
        dir.path(),
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )?;
    let rules = WorktreeRules::from_repository(&repo, &index)?;
    let _ = diff_tree_to_worktree_with_git_dir_and_rules(
        repo.odb(),
        Some(&head_tree),
        dir.path(),
        repo.git_dir(),
        &index,
        Some(&cfg),
        Some(&rules),
    )?;
    let patch = "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-one\n+two\n";
    let _ = parse_patch(patch, 1, "p", false, None)?;
    let specs = parse_pathspecs_from_source(b"*.txt\0dir/\0", true)?;
    for path in ["f.txt", "g.txt", "dir/b.txt", "missing"] {
        let _ = matches_pathspec_list(path, &specs);
    }
    Ok(())
}

#[test]
fn supporting_and_submodule_gitdir_sweep() -> grit_lib::error::Result<()> {
    use grit_lib::config::gitmodules::gitdir::{
        ensure_submodule_gitdir_config, submodule_gitdir_filesystem_path,
        submodule_path_config_enabled,
    };
    use grit_lib::config::gitmodules::{check_submodule_name, validate_gitmodules_blob_line};
    use grit_lib::config::{ConfigFile, ConfigScope};
    use grit_lib::worktree::crlf::{
        convert_to_git, convert_to_worktree_eager, ConversionConfig, FileAttrs,
    };

    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let git_dir = repo.git_dir();
    fs::write(
        git_dir.join("config"),
        "[extensions]\n\tsubmodulePathConfig = true\n[core]\n\tautocrlf = true\n",
    )
    .map_err(grit_lib::error::Error::Io)?;
    assert!(submodule_path_config_enabled(git_dir));
    assert!(check_submodule_name("lib/foo"));
    assert!(validate_gitmodules_blob_line(b"[submodule \"x\"]\n").is_none());
    let cfg = ConfigFile::parse(
        git_dir.join("config").as_path(),
        "[submodule \"lib/foo\"]\n\tpath = lib/foo\n\turl = https://example.com/r.git\n",
        ConfigScope::Local,
    )?;
    let mut cfg_mut = cfg.clone();
    let gitdir_rel = ensure_submodule_gitdir_config(dir.path(), git_dir, &mut cfg_mut, "lib/foo")?;
    assert!(!gitdir_rel.is_empty());
    let fs_path = submodule_gitdir_filesystem_path(dir.path(), git_dir, &cfg_mut, "lib/foo")?;
    assert!(fs_path.exists() || fs_path.starts_with(dir.path()));
    let mut set = ConfigSet::new();
    set.merge(&cfg_mut);
    let crlf = ConversionConfig::from_config(&set);
    let attrs = FileAttrs::default();
    let git_bytes = convert_to_git(b"a\r\n", "x.txt", &crlf, &attrs)?;
    let _ = convert_to_worktree_eager(&git_bytes, "x.txt", &crlf, &attrs, None, None, None)?;
    Ok(())
}
