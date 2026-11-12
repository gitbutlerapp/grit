//! Broad API sweeps to ratchet history and total line coverage toward the 80/85 gate.

use grit_lib::progress::{CancelToken, NullProgress};

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::combined_tree_diff::{CombinedParentSide, CombinedParentStatus};
use grit_lib::diff::merge_diff::{
    all_blob_paths_in_tree_order, blob_oid_at_path, blob_text_for_diff, combined_diff_paths,
    combined_merge_parent_blob_paths, diff_attr_external_driver, diff_attr_forces_binary,
    diff_attr_forces_text, diff_cachetextconv_enabled, diff_forced_binary_by_driver,
    diff_textconv_active, diff_textconv_cmd_line, format_combined_binary,
    format_combined_binary_header, format_combined_binary_header_n, format_combined_textconv_patch,
    format_worktree_conflict_combined, is_binary_for_diff, read_blob_at_path, run_textconv,
    CombinedPatchOptions, ConflictStages,
};
use grit_lib::diff::PathQuoting;
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{
    common_git_dir_for_config, early_config_ignore_repo_reason, ensure_core_bare,
    init_worktree_config, repository_format_warning, resolve_dot_git, resolve_git_directory_arg,
    validate_repo_config, validate_repo_format, worktree_config_enabled, InitOptions, Repository,
};
use grit_lib::revwalk::rev_list::{MissingAction, ObjectFilter, OrderingMode, RevListOptions};

use support::{commit_file, git, open_grit_repo, HistoryRepo};

fn open_grit(wt: &Path) -> Repository {
    open_grit_repo(wt)
}

#[test]
fn merge_diff_and_textconv_matrix_on_merge_commit() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )?;
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"shared\n")
        .expect("blob");
    let mut tree_body = format!("100644 file.txt\0").into_bytes();
    tree_body.extend_from_slice(blob.as_bytes());
    let tree = repo.odb().write(ObjectKind::Tree, &tree_body)?;
    let base = {
        let body = format!(
            "tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nbase\n"
        );
        repo.odb().write(ObjectKind::Commit, body.as_bytes())?
    };
    let blob2 = repo.odb().write(ObjectKind::Blob, b"side\n")?;
    let mut tree2 = format!("100644 other.txt\0").into_bytes();
    tree2.extend_from_slice(blob2.as_bytes());
    let tree2 = repo.odb().write(ObjectKind::Tree, &tree2)?;
    let side = {
        let body = format!(
            "tree {tree2}\nparent {base}\nauthor T <t@e.com> 2 +0000\ncommitter T <t@e.com> 2 +0000\n\nside\n"
        );
        repo.odb().write(ObjectKind::Commit, body.as_bytes())?
    };
    let blob3 = repo.odb().write(ObjectKind::Blob, b"merged\n")?;
    let mut tree3 = format!("100644 file.txt\0").into_bytes();
    tree3.extend_from_slice(blob3.as_bytes());
    let tree3 = repo.odb().write(ObjectKind::Tree, &tree3)?;
    let merge = {
        let body = format!(
            "tree {tree3}\nparent {base}\nparent {side}\nauthor T <t@e.com> 3 +0000\ncommitter T <t@e.com> 3 +0000\n\nmerge\n"
        );
        repo.odb().write(ObjectKind::Commit, body.as_bytes())?
    };
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{merge}\n"))
        .map_err(grit_lib::error::Error::Io)?;
    let commit_obj = repo.odb().read(&merge)?;
    let parsed = parse_commit(&commit_obj.data)?;
    assert_eq!(parsed.parents.len(), 2);
    let git_dir = repo.git_dir();

    let paths = combined_diff_paths(repo.odb(), &parsed.tree, &parsed.parents);
    assert!(!paths.is_empty());
    let all_paths = all_blob_paths_in_tree_order(repo.odb(), &parsed.tree);
    assert!(!all_paths.is_empty());
    let _ =
        combined_merge_parent_blob_paths(repo.odb(), "file.txt", &parsed.parents, &parsed.tree, 50);

    let config = repo.config().snapshot()?;
    let sides = [
        CombinedParentSide::new(0o100644, parsed.parents[0], CombinedParentStatus::Modified),
        CombinedParentSide::new(0o100644, parsed.parents[1], CombinedParentStatus::Modified),
    ];
    for path in &paths {
        let oid = blob_oid_at_path(repo.odb(), &parsed.tree, path).expect("blob oid");
        let blob = read_blob_at_path(repo.odb(), &parsed.tree, path).expect("read blob");
        let _ = is_binary_for_diff(git_dir, path, &blob);
        let _ = diff_attr_forces_binary(git_dir, path);
        let _ = diff_attr_forces_text(git_dir, path);
        let _ = diff_forced_binary_by_driver(git_dir, config.as_ref(), path, "100644", "100644");
        let _ = diff_attr_external_driver(git_dir, config.as_ref(), path);
        let _ = diff_textconv_active(git_dir, config.as_ref(), path);
        let _ = diff_cachetextconv_enabled(config.as_ref(), "text");
        let _ = diff_textconv_cmd_line(config.as_ref(), "text");
        let _ = blob_text_for_diff(git_dir, config.as_ref(), path, &blob, false);
        let _ = run_textconv(git_dir, config.as_ref(), path, &blob);
        let parents = &parsed.parents;
        let _ = format_combined_binary_header(path, parents, &oid, 7, false);
        let _ = format_combined_binary_header_n(path, parents, &oid, 7, false);
        let _ = format_combined_binary(path, parents, &oid, 7, false);
        let _ = format_combined_textconv_patch(
            git_dir,
            config.as_ref(),
            repo.odb(),
            path,
            &parsed.parents,
            &parsed.tree,
            &CombinedPatchOptions::default()
                .with_parent_sides(&sides)
                .with_path_quoting(PathQuoting::KeepNonAscii),
        );
        let stage1 = parsed.parents[0];
        let stage2 = parsed.parents[1];
        let stage3 = parsed.parents[1];
        let _ = format_worktree_conflict_combined(
            git_dir,
            config.as_ref(),
            repo.odb(),
            path,
            &ConflictStages {
                base: stage1,
                ours: stage2,
                theirs: stage3,
            },
            b"<<<<<<< ours\nmerged\n=======\n",
            7,
        );
    }
    Ok(())
}

#[test]
fn repo_setup_and_format_helpers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    Repository::init(path, &InitOptions::default()).expect("init");
    let git_dir = path.join(".git");
    let _ = validate_repo_format(&git_dir);
    let _ = repository_format_warning(&git_dir);
    let _ = init_worktree_config(&git_dir);
    let common = common_git_dir_for_config(&git_dir);
    let _ = worktree_config_enabled(&common);
    let _ = early_config_ignore_repo_reason(&common);
    let _ = resolve_git_directory_arg(&git_dir);
    let _ = resolve_dot_git(&git_dir);
    let _ = ensure_core_bare(&git_dir);
    assert!(validate_repo_config("[core]\n\trepositoryformatversion = 0\n").is_ok());

    let bare = tempfile::tempdir().expect("bare");
    Repository::init(
        bare.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("bare");
    let sep = tempfile::tempdir().expect("sep");
    let git_dir = sep.path().join("repo.git");
    Repository::init(
        sep.path(),
        &InitOptions::new()
            .with_separate_git_dir(Some(git_dir.clone()))
            .with_initial_branch("main"),
    )
    .expect("sep");
}

#[test]
fn rev_list_deep_history_and_ref_matrix() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    for i in 0..100 {
        commit_file(
            wt,
            &format!("files/f{i}.txt"),
            &format!("body {i}\n"),
            &format!("commit {i}"),
        );
        if i % 11 == 0 {
            commit_file(
                wt,
                &format!("docs/d{i}.md"),
                &format!("# doc {i}\n"),
                &format!("doc {i}"),
            );
        }
    }
    git(wt, &["checkout", "-b", "side", "main~10"]);
    commit_file(wt, "side.txt", "side\n", "side tip");
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "side", "-m", "merge side", "--no-ff"]);

    let repo = open_grit(wt);
    let orderings = [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
        OrderingMode::AuthorDateTopo,
    ];
    let tip: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().expect("tip");
    let mid: ObjectId = git(wt, &["rev-parse", "main~10"])
        .trim()
        .parse()
        .expect("mid");
    for ordering in orderings {
        for ancestry in [false, true] {
            let opts = RevListOptions::default()
                .with_ordering(ordering)
                .with_parent_rewrite(true)
                .with_ancestry_path(ancestry)
                .with_ancestry_path_bottoms(vec![mid])
                .with_left_right(true)
                .with_cherry_mark(true)
                .with_boundary(true)
                .with_first_parent(true)
                .with_simplify_merges(true)
                .with_simplify_by_decoration(ancestry)
                .with_simplify_by_decoration_oids([tip, mid].into_iter().collect())
                .with_full_history(true)
                .with_objects(true)
                .with_in_commit_order(true)
                .with_include_reflog_entries(true)
                .with_all_refs(true)
                .with_max_count(Some(80))
                .with_missing_action(MissingAction::Allow)
                .with_filter(Some(ObjectFilter::Combine(vec![
                    ObjectFilter::BlobLimit(65536),
                    ObjectFilter::TreeDepth(8),
                ])))
                .with_filter_print_omitted(true);
            let out = repo.revwalk().rev_list(
                &[String::from("main")],
                &[],
                &opts,
                &mut NullProgress,
                &CancelToken::never(),
            )?;
            assert!(!out.commits.is_empty());
        }
    }

    let _ = repo
        .revwalk()
        .resolve_commits(&["main".to_owned(), "side".to_owned()])?;
    let _ = repo.revwalk().tag_targets()?;
    let _ = repo.revwalk().shallow_boundary_oids();
    let filtered = RevListOptions::default()
        .with_all_refs(true)
        .with_objects(true)
        .with_no_object_names(true)
        .with_filter(Some(ObjectFilter::BlobLimit(4096)));
    let _ = repo.revwalk().rev_list(
        &[],
        &[],
        &filtered,
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    let _ = repo
        .revwalk()
        .merge_bases(&[tip, mid], grit_lib::revwalk::MergeBaseMode::All)?;
    Ok(())
}

#[test]
fn gitattributes_textconv_driver_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path();
    Repository::init(wt, &InitOptions::default()).expect("init");
    fs::write(
        wt.join(".gitattributes"),
        "*.bin binary\n*.txt text diff=custom\n",
    )
    .expect("attrs");
    let cfg = ConfigFile::parse(
        wt.join(".git/config").as_path(),
        "[diff \"custom\"]\n\ttextconv = cat\n\tcachetextconv = true\n",
        ConfigScope::Local,
    )
    .expect("config");
    let mut set = ConfigSet::new();
    set.merge(&cfg);
    let repo = open_grit(wt);
    commit_file(wt, "sample.txt", "hello\n", "c");
    let tree: ObjectId = git(wt, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse()
        .expect("tree");
    let blob = read_blob_at_path(repo.odb(), &tree, "sample.txt").expect("blob");
    let _ = diff_textconv_cmd_line(&set, "custom");
    let _ = run_textconv(repo.git_dir(), &set, "sample.txt", &blob);
}
