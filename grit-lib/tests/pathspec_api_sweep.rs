//! Exercise [`grit_lib::worktree::pathspec`] helpers for line coverage.

use grit_lib::worktree::pathspec::{
    bloom_lookup_prefix, bloom_lookup_prefix_with_cwd, context_from_mode_octal,
    extend_pathspec_list_implicit_cwd, has_glob_chars, literal_pathspecs_enabled, matches_pathspec,
    matches_pathspec_list, matches_pathspec_list_for_object, matches_pathspec_list_with_context,
    matches_pathspec_set_for_object, matches_pathspec_set_for_object_ls_tree,
    matches_pathspec_with_context, parse_pathspecs_from_source, path_allowed_by_pathspec_list,
    pathspec_contributes_match, pathspec_exclude_matches, pathspec_has_top, pathspec_is_exclude,
    pathspec_matches, pathspec_wants_descent_into_tree, pathspecs_allow_bloom, simple_length,
    validate_attr_pathspecs, validate_global_pathspec_flags, PathspecMatchContext,
};

#[test]
fn pathspec_parse_match_and_bloom_matrix() {
    let _ = validate_global_pathspec_flags();
    let specs = parse_pathspecs_from_source(b"src/\0docs/\0", true).expect("parse");
    assert!(!specs.is_empty());
    let _ = validate_attr_pathspecs(&[":(attr:binary)".to_owned()]);
    assert!(has_glob_chars("*.rs"));
    let _ = simple_length(":(glob)foo");
    let _ = simple_length("foo");
    let list = vec!["src/".to_owned(), ":!exclude/".to_owned()];
    let _ = extend_pathspec_list_implicit_cwd(&list, Some("pkg"));
    assert!(path_allowed_by_pathspec_list(&list, "src/main.rs"));
    assert!(matches_pathspec_list("src/lib.rs", &list));
    assert!(matches_pathspec_list_with_context(
        "src/lib.rs",
        &list,
        PathspecMatchContext::default()
    ));
    assert!(matches_pathspec("src/*.rs", "src/a.rs"));
    assert!(matches_pathspec_with_context(
        "src/*.rs",
        "src/a.rs",
        PathspecMatchContext::default()
    ));
    assert!(pathspec_matches(":(glob)*.txt", "foo.txt"));
    assert!(!pathspec_is_exclude("src/"));
    assert!(pathspec_is_exclude(":!src/"));
    assert!(pathspec_has_top(":/README"));
    assert!(pathspec_wants_descent_into_tree("src/", "src"));
    assert!(pathspec_contributes_match("src/", "src/a.rs"));
    assert!(!pathspec_exclude_matches("src/", "src/a.rs"));
    let _ctx = context_from_mode_octal("100644");
    let mode = 0o100644;
    let rules: &[grit_lib::worktree::crlf::AttrRule] = &[];
    let _ = matches_pathspec_set_for_object(&list, "src/a.rs", mode, rules);
    let _ = matches_pathspec_set_for_object_ls_tree(&list, "src", mode, rules);
    assert!(pathspecs_allow_bloom(&["src/".to_owned()]));
    let _ = bloom_lookup_prefix("src/");
    let _ = bloom_lookup_prefix_with_cwd("src/", Some("pkg"));
    let _ = matches_pathspec_list_for_object("src/a.rs", mode, rules, &list);
    let _ = literal_pathspecs_enabled();
}
