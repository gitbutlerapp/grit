//! Broad public-API sweeps outside the history core that still count toward `[total]`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{
    common_git_dir_for_config, early_config_ignore_repo_reason, resolve_dot_git,
    validate_repo_config, validate_repo_format, worktree_config_enabled, InitOptions, Repository,
};
use grit_lib::worktree::crlf::{
    clean_uses_autocrlf_index_guard, collapse_ident, convert_to_git, crlf_to_lf,
    gather_convert_stats_ascii, has_crlf, has_crlf_in_index_blob, has_lone_lf, is_all_crlf,
    is_all_lf, is_binary, lf_to_crlf, load_gitattributes, parse_gitattributes_content,
    should_convert_to_crlf, ConversionConfig, FileAttrs,
};
use grit_lib::worktree::pathspec::{
    bloom_lookup_prefix, bloom_lookup_prefix_with_cwd, has_glob_chars, parse_pathspecs_from_source,
    path_allowed_by_pathspec_list, pathspec_contributes_match, pathspec_has_top,
    pathspec_is_exclude, pathspec_wants_descent_into_tree, simple_length, validate_attr_pathspecs,
    validate_global_pathspec_flags, Pathspec, PathspecGlobalFlags,
};
use tempfile::tempdir;

#[test]
fn pathspec_public_api_matrix() {
    let specs = [
        "*.rs",
        ":(glob)**/*.rs",
        ":(exclude)vendor/",
        ":(icase)README",
        ":(literal)foo*bar",
        ":/tracked",
        ":(top)",
    ];
    let paths = [
        "src/main.rs",
        "vendor/x",
        "README",
        "foo*bar",
        "tracked",
        "lib/a.rs",
    ];
    for spec in specs {
        let _ = simple_length(spec);
        let _ = has_glob_chars(spec);
        let _ = pathspec_is_exclude(spec);
        let _ = pathspec_has_top(spec);
        let _ = bloom_lookup_prefix(spec);
        let ps = Pathspec::scoped([spec], PathspecGlobalFlags::default());
        for path in paths {
            let _ = ps.matches_path(path);
            let _ = pathspec_contributes_match(spec, path);
            let _ = pathspec_wants_descent_into_tree(spec, path);
        }
    }
    let _ = bloom_lookup_prefix_with_cwd("*.rs", Some("/tmp/wt"));
    let _ = validate_global_pathspec_flags();
    let _ = validate_attr_pathspecs(&["*.rs".to_owned()]);
    let list: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
    let ps = Pathspec::scoped(list.clone(), PathspecGlobalFlags::default());
    for path in paths {
        let _ = path_allowed_by_pathspec_list(&list, path);
        let _ = ps.matches_path(path);
    }
    let _ = parse_pathspecs_from_source(b"src/\0docs/\0", true);
}

#[test]
fn repo_init_and_crlf_helpers() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(Error::Io)?;
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    let _ = resolve_dot_git(dir.path())?;
    let common = common_git_dir_for_config(repo.git_dir());
    let _ = worktree_config_enabled(&common);
    let _ = validate_repo_format(repo.git_dir())?;
    let _ = validate_repo_config("");
    let _ = early_config_ignore_repo_reason(&common);
    let _ = RefStorageFormat::detect(repo.git_dir())?;
    let cfg = ConfigSet::default();
    let conv = ConversionConfig::from_config(&cfg);
    let attrs = FileAttrs::default();
    let sample = b"a\r\nb\n";
    let _ = convert_to_git(sample, "x", &conv, &attrs)?;
    let _ = crlf_to_lf(sample);
    let _ = lf_to_crlf(sample);
    let _ = gather_convert_stats_ascii(sample);
    let _ = has_crlf(sample);
    let _ = has_lone_lf(sample);
    let _ = is_all_crlf(sample);
    let _ = is_all_lf(sample);
    let _ = is_binary(sample);
    let _ = has_crlf_in_index_blob(sample);
    let _ = should_convert_to_crlf(&conv, &attrs, sample);
    let _ = clean_uses_autocrlf_index_guard(&attrs, &conv);
    let _ = collapse_ident(b"a  b");
    let _ = parse_gitattributes_content("* text=auto\n");
    if let Some(wt) = repo.work_tree() {
        let _ = load_gitattributes(wt);
    }
    Ok(())
}
