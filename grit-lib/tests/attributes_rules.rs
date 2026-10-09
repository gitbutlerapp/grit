//! Gitattributes oracle tests (upstream `t/t0003-attributes.sh` scenarios).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::attributes::{
    builtin_objectmode_index, builtin_objectmode_worktree, builtin_warnings_for_rules,
    collect_attrs_for_path, is_reserved_builtin_name, load_gitattributes_bare,
    load_gitattributes_for_check_attr_cached, load_gitattributes_for_check_attr_source,
    load_gitattributes_for_diff, load_gitattributes_from_index, load_gitattributes_from_tree,
    parse_gitattributes_file_content, parse_gitattributes_file_content_with_base,
    quote_path_for_check_attr, resolve_attr_treeish, resolve_tree_oid, validate_rules_for_add,
    AttrValue, MAX_ATTR_FILE_BYTES, MAX_ATTR_LINE_BYTES,
};
use grit_lib::error::Error;
use grit_lib::index::Index;
use grit_lib::repo::Repository;
use support::{
    assert_attrs_match, grit_attr_value, grit_repo, hermetic_git, hermetic_git_ok, init_git_repo,
    load_cached_parsed, load_source_parsed, load_worktree_parsed, rel_path_for_check,
    setup_t0003_tags, setup_t0003_worktree, t0003_corpus_paths, t0003_source_paths,
};

#[test]
fn t0003_attrs_match_git_check_attr_worktree_cached_and_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    setup_t0003_worktree(wt);
    setup_t0003_tags(wt);
    hermetic_git_ok(
        wt,
        &[
            "add",
            ".gitattributes",
            "a/.gitattributes",
            "a/b/.gitattributes",
        ],
    );

    let repo = grit_repo(wt);
    let paths = t0003_corpus_paths();
    let path_refs: Vec<&str> = paths.iter().copied().collect();

    let worktree = load_worktree_parsed(&repo, wt);
    assert_attrs_match(wt, &repo, &worktree, "test", &path_refs, &[]);

    let cached = load_cached_parsed(&repo);
    assert_attrs_match(wt, &repo, &cached, "test", &path_refs, &["--cached"]);

    for treeish in ["tag-1", "tag-2"] {
        let parsed = load_source_parsed(&repo, treeish);
        let git_extra = ["--source", treeish];
        assert_attrs_match(
            wt,
            &repo,
            &parsed,
            "test",
            &t0003_source_paths(),
            &git_extra,
        );
    }
}

#[test]
fn t0003_macro_rules_only_top_level() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "[attr]rootmacro test=fromroot\n").expect("root");
    std::fs::create_dir_all(wt.join("sub")).expect("mkdir");
    std::fs::write(
        wt.join("sub/.gitattributes"),
        "[attr]bad test=ignored\nf rootmacro\n",
    )
    .expect("nested");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert_attrs_match(wt, &repo, &parsed, "test", &["sub/f"], &[]);
    let nested_only = parse_gitattributes_file_content_with_base(
        "[attr]bad test=ignored\n",
        "sub/.gitattributes",
        "sub",
    );
    assert!(
        nested_only
            .warnings
            .iter()
            .any(|w| w.contains("not allowed")),
        "expected nested [attr] warning, got {:?}",
        nested_only.warnings
    );
    assert!(nested_only.macros.defs.is_empty());
}

#[test]
fn t0003_binary_macro_expansion_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "file binary\n").expect("ga");
    std::fs::write(wt.join("file"), b"x").expect("file");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    for attr in ["binary", "text", "diff", "merge"] {
        assert_attrs_match(wt, &repo, &parsed, attr, &["file"], &[]);
    }
}

#[test]
fn t0003_negative_pattern_emits_warning() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "!f test=bar\n").expect("ga");
    let out = hermetic_git(wt, &["check-attr", "test", "--", "!f"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Negative patterns are ignored"));
    let parsed = parse_gitattributes_file_content("!f test=bar\n", ".gitattributes");
    assert!(parsed
        .warnings
        .iter()
        .any(|w| w.contains("Negative patterns are ignored")));
}

#[test]
fn t0003_escaped_bang_pattern_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "\\!f test=foo\n").expect("ga");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert_attrs_match(wt, &repo, &parsed, "test", &["!f"], &[]);
}

#[test]
fn t0003_doublestar_patterns_match_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "**/f foo=bar\n").expect("ga");
    let paths = ["f", "a/f", "a/b/f", "a/b/c/f"];
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert_attrs_match(wt, &repo, &parsed, "foo", &paths, &[]);
}

#[test]
fn t0003_a_doublestar_f_pattern_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "a**f foo=bar\n").expect("ga");
    let paths = ["f", "af", "axf", "a/f", "a/b/f"];
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert_attrs_match(wt, &repo, &parsed, "foo", &paths, &[]);
}

#[test]
fn t0003_open_quoted_pathname_line_ignored() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "\"a test=a\n").expect("ga");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert_attrs_match(wt, &repo, &parsed, "test", &["a"], &[]);
}

#[test]
fn t0003_ignorecase_worktree_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    setup_t0003_worktree(wt);
    hermetic_git_ok(wt, &["config", "core.ignorecase", "true"]);
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    let cases = [
        ("F", "f"),
        ("a/F", "f"),
        ("a/G", "a/g"),
        ("a/b/H", "a/b/h"),
        ("a/E/f", "A/e/F"),
    ];
    for (path, _expect) in cases {
        assert_attrs_match(wt, &repo, &parsed, "test", &[path], &[]);
    }
}

#[test]
fn t0003_quote_path_for_check_attr_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "e\\\" test=quoted\n").expect("ga");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    let git_line = hermetic_git_ok(wt, &["check-attr", "test", "--", "e\""]);
    assert!(git_line.contains("quoted"));
    assert_eq!(grit_attr_value(&parsed, &repo, "e\"", "test"), "quoted");
    let rel = rel_path_for_check(&repo, "e\"");
    assert_eq!(quote_path_for_check_attr(&rel), "\"e\\\"\"");
}

#[test]
fn t0003_normalize_rel_path_and_relative_worktree_paths() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    setup_t0003_worktree(wt);
    let repo = grit_repo(wt);
    assert_eq!(rel_path_for_check(&repo, "./f"), "f");
    assert_eq!(rel_path_for_check(&repo, "a/./g"), "a/g");
    assert_eq!(rel_path_for_check(&repo, "a/c/../b/g"), "a/b/g");
}

#[test]
fn t0003_overlong_line_skipped_with_warning() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let line = format!("path {:0width$}", 1, width = MAX_ATTR_LINE_BYTES);
    std::fs::write(wt.join(".gitattributes"), format!("{line}\n")).expect("ga");
    let out = hermetic_git(wt, &["check-attr", "--all", "path"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ignoring overly long attributes line"));
    let parsed = parse_gitattributes_file_content(&format!("{line}\n"), ".gitattributes");
    assert!(parsed.rules.is_empty());
    assert!(parsed.warnings.iter().any(|w| w.contains("overly long")));
}

#[test]
fn t0003_builtin_objectmode_worktree_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join("normal"), b"").expect("normal");
    std::fs::create_dir(wt.join("dir")).expect("dir");
    let repo = grit_repo(wt);
    let git_line = hermetic_git_ok(wt, &["check-attr", "builtin_objectmode", "--", "normal"]);
    if git_line.contains("unspecified") {
        eprintln!("SKIP: system git lacks builtin_objectmode check-attr support");
        return;
    }
    for (path, grit_mode) in [
        ("normal", builtin_objectmode_worktree(&repo, "normal")),
        ("dir", builtin_objectmode_worktree(&repo, "dir")),
    ] {
        let git_line = hermetic_git_ok(wt, &["check-attr", "builtin_objectmode", "--", path]);
        let git_val = git_line.trim().rsplit(':').next().unwrap_or("").trim();
        assert_eq!(grit_mode.as_deref(), Some(git_val));
    }
}

#[test]
fn t0003_builtin_objectmode_cached_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join("normal"), b"").expect("normal");
    hermetic_git_ok(wt, &["add", "normal"]);
    let git_line = hermetic_git_ok(
        wt,
        &[
            "check-attr",
            "--cached",
            "builtin_objectmode",
            "--",
            "normal",
        ],
    );
    if git_line.contains("unspecified") {
        eprintln!("SKIP: system git lacks builtin_objectmode check-attr support");
        return;
    }
    let index = Index::load(&wt.join(".git/index")).expect("index");
    let grit_mode = builtin_objectmode_index(&index, "normal");
    let git_val = git_line.trim().rsplit(':').next().unwrap_or("").trim();
    assert_eq!(grit_mode.as_deref(), Some(git_val));
}

#[test]
fn t0003_validate_rules_for_add_rejects_bad_builtin_names() {
    let parsed = parse_gitattributes_file_content("foo* builtin_foo\n", ".gitattributes");
    let err = validate_rules_for_add(&parsed.rules, ".gitattributes").unwrap_err();
    assert!(err.contains("builtin_foo is not a valid attribute name"));
    let warnings = builtin_warnings_for_rules(&parsed.rules, ".gitattributes");
    assert!(warnings.iter().any(|w| w.contains("builtin_foo")));
}

#[test]
fn t0003_is_reserved_builtin_name() {
    assert!(!is_reserved_builtin_name("text"));
    // Name is historical: returns true only for the allowed real builtin name.
    assert!(is_reserved_builtin_name("builtin_objectmode"));
    assert!(!is_reserved_builtin_name("builtin_foo"));
}

#[test]
fn t0003_info_attributes_precedence_over_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "f test=root\n").expect("root");
    std::fs::create_dir_all(wt.join(".git/info")).expect("info");
    std::fs::write(wt.join(".git/info/attributes"), "f test=info\n").expect("info");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert_eq!(grit_attr_value(&parsed, &repo, "f", "test"), "info");
    assert_attrs_match(wt, &repo, &parsed, "test", &["f"], &[]);
}

#[test]
fn t0003_load_gitattributes_from_index_matches_cached_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    setup_t0003_worktree(wt);
    hermetic_git_ok(
        wt,
        &[
            "add",
            ".gitattributes",
            "a/.gitattributes",
            "a/b/.gitattributes",
        ],
    );
    let repo = grit_repo(wt);
    let cached = load_cached_parsed(&repo);
    assert_attrs_match(
        wt,
        &repo,
        &cached,
        "test",
        &t0003_corpus_paths(),
        &["--cached"],
    );
}

#[test]
fn t0003_macro_notest_clears_test_on_d_yes() {
    let mut merged = parse_gitattributes_file_content("[attr]notest !test\n", ".gitattributes");
    let mut nested =
        parse_gitattributes_file_content_with_base("d/yes notest\n", "a/b/.gitattributes", "a/b");
    merged.rules.append(&mut nested.rules);
    let map = collect_attrs_for_path(&merged.rules, &merged.macros, "a/b/d/yes", false);
    assert!(!map.contains_key("test"));
    assert_eq!(map.get("notest"), Some(&AttrValue::Set));
}

#[test]
fn t0003_expensive_large_gitattributes_file_skipped() {
    if std::env::var("GRIT_RUN_EXPENSIVE_ATTR_TESTS").is_err() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let size = MAX_ATTR_FILE_BYTES + 1024;
    let data = vec![b'x'; size];
    std::fs::write(wt.join(".gitattributes"), &data).expect("large");
    let out = hermetic_git(wt, &["check-attr", "--all", "path"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ignoring overly large"));
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert!(parsed.rules.is_empty());
}

#[test]
fn t0003_crlf_in_gitattributes_blob_from_index() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let content = "* text eol=crlf\r\n";
    std::fs::write(wt.join(".gitattributes"), content).expect("crlf ga");
    hermetic_git_ok(wt, &["add", ".gitattributes"]);
    let repo = grit_repo(wt);
    let index = Index::load(&wt.join(".git/index")).expect("index");
    let parsed = load_gitattributes_from_index(&index, &repo.odb, wt).expect("from index");
    let map = collect_attrs_for_path(&parsed.rules, &parsed.macros, "any.txt", false);
    assert_eq!(map.get("text"), Some(&AttrValue::Set));
}

#[test]
fn t0003_core_attributesfile_and_global_precedence() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let global = wt.join("global-attrs");
    std::fs::write(&global, "f test=global\n").expect("global");
    hermetic_git_ok(
        wt,
        &[
            "config",
            "core.attributesfile",
            global.to_str().expect("utf8"),
        ],
    );
    std::fs::write(wt.join(".gitattributes"), "f test=root\n").expect("root");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert_attrs_match(wt, &repo, &parsed, "test", &["f"], &[]);
    std::fs::write(wt.join(".gitattributes"), "f test=precedence\n").expect("root2");
    let parsed2 = load_worktree_parsed(&repo, wt);
    assert_eq!(grit_attr_value(&parsed2, &repo, "f", "test"), "precedence");
}

#[test]
fn t0003_bare_repo_info_attributes_and_index() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join(".gitattributes"), "f test=tracked\n").expect("ga");
    hermetic_git_ok(wt, &["add", ".gitattributes"]);
    let bare_dir = wt.join("bare.git");
    hermetic_git_ok(
        wt,
        &["clone", "--bare", ".", bare_dir.to_str().expect("utf8")],
    );
    std::fs::write(bare_dir.join("info/attributes"), "f test=info\n").expect("info");
    let repo = Repository::open(&bare_dir, None).expect("bare open");
    let bare_parsed = load_gitattributes_bare(&repo).expect("bare load");
    assert_eq!(grit_attr_value(&bare_parsed, &repo, "f", "test"), "info");
}

#[test]
fn t0003_load_gitattributes_from_tree_and_diff_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    setup_t0003_tags(wt);
    let repo = grit_repo(wt);
    let oid = resolve_tree_oid(&repo, "tag-1").expect("tree");
    let from_tree = load_gitattributes_from_tree(&repo, &oid).expect("from tree");
    let from_source = load_gitattributes_for_check_attr_source(&repo, &oid).expect("source");
    assert_attrs_match(
        wt,
        &repo,
        &from_source,
        "test",
        &["foo/bar/f"],
        &["--source", "tag-1"],
    );
    assert!(!from_tree.rules.is_empty());
    hermetic_git_ok(wt, &["config", "attr.tree", "tag-2"]);
    let diff_loaded = load_gitattributes_for_diff(&repo).expect("diff load");
    assert_eq!(
        grit_attr_value(&diff_loaded, &repo, "foo/bar/a/i", "test"),
        "m"
    );
}

#[test]
fn t0003_resolve_attr_treeish_and_tree_oid_errors() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let repo = grit_repo(wt);
    assert!(resolve_tree_oid(&repo, "not-a-ref").is_err());
    let (spec, ignore) = resolve_attr_treeish(&repo, Some("HEAD")).expect("treeish");
    assert_eq!(spec.as_deref(), Some("HEAD"));
    assert!(!ignore);
    hermetic_git_ok(wt, &["config", "attr.tree", "HEAD"]);
    let (spec2, ignore2) = resolve_attr_treeish(&repo, None).expect("cfg tree");
    assert_eq!(spec2.as_deref(), Some("HEAD"));
    assert!(ignore2);

    use grit_lib::environment::{Environment, RepositoryOptions};
    use std::ffi::OsString;
    let mut env = Environment::from_vars(
        [(
            OsString::from("GIT_ATTR_SOURCE"),
            OsString::from("not-a-valid-ref"),
        )],
        wt.to_path_buf(),
    );
    env.git_config_system = Some(null_dev().to_string());
    env.git_config_global = Some(null_dev().to_string());
    let options = RepositoryOptions::with_environment(env);
    let repo2 = Repository::open_with(&options, &wt.join(".git"), Some(wt)).expect("open2");
    let err = load_gitattributes_for_diff(&repo2).unwrap_err();
    assert!(matches!(err, Error::InvalidRef(_)));
}

fn null_dev() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

#[test]
fn t0003_symlink_gitattributes_in_tree_skipped_with_warning() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    std::fs::write(wt.join("attr-src"), "* test=set\n").expect("src");
    std::os::unix::fs::symlink("attr-src", wt.join(".gitattributes")).expect("symlink");
    let repo = grit_repo(wt);
    let parsed = load_worktree_parsed(&repo, wt);
    assert!(parsed.rules.is_empty());
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.contains("symbolic links") || w.contains("Too many levels")),
        "warnings: {:?}",
        parsed.warnings
    );
}

#[test]
fn t0003_gitattributes_blob_literal_backslash_n_expanded_from_index() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let blob = "* text\\n";
    std::fs::write(wt.join(".gitattributes"), blob).expect("ga");
    hermetic_git_ok(wt, &["add", ".gitattributes"]);
    let repo = grit_repo(wt);
    let index = Index::load(&wt.join(".git/index")).expect("index");
    let parsed = load_gitattributes_from_index(&index, &repo.odb, wt).expect("from index");
    let map = collect_attrs_for_path(&parsed.rules, &parsed.macros, "file", false);
    assert_eq!(map.get("text"), Some(&AttrValue::Set));
}

#[test]
fn t0003_path_outside_repository_errors() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let repo = grit_repo(wt);
    assert!(grit_lib::attributes::path_relative_to_worktree(&repo, "/outside/path").is_err());
}

#[test]
fn t0003_oversized_gitattributes_blob_in_index_skipped() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let blob = vec![b'x'; MAX_ATTR_FILE_BYTES + 1];
    let mut child = std::process::Command::new("git")
        .current_dir(wt)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_SYSTEM", null_dev())
        .env("GIT_CONFIG_GLOBAL", null_dev())
        .spawn()
        .expect("hash-object");
    std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), &blob).expect("write");
    let out = child.wait_with_output().expect("wait");
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    hermetic_git_ok(
        wt,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{hash},.gitattributes"),
        ],
    );
    let repo = grit_repo(wt);
    let parsed = load_cached_parsed(&repo);
    assert!(parsed.rules.is_empty());
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.contains("ignoring overly large gitattributes blob")),
        "{:?}",
        parsed.warnings
    );
}

#[test]
fn t0003_cached_without_index_entries_is_empty() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);
    let repo = grit_repo(wt);
    let cached = load_gitattributes_for_check_attr_cached(&repo).expect("cached");
    assert!(cached.rules.is_empty());
}
