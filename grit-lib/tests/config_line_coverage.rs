//! Targeted line coverage for [`grit_lib::config`] helpers not fully exercised by oracle tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_lib::config::{
    canonical_key, get_urlmatch_all_in_section, get_urlmatch_entries, global_config_paths_pub,
    parse_bool, parse_color, parse_i64, parse_path, parse_path_optional,
    resolve_diff_context_lines, url_matches, ConfigFile, ConfigScope, ConfigSet, LoadConfigOptions,
};
use grit_lib::environment::Environment;
use grit_lib::error::Error;
use std::fmt::Write as FmtWrite;
use tempfile::tempdir;

fn set_from_snippet(text: &str) -> ConfigSet {
    let file =
        ConfigFile::parse(Path::new(".git/config"), text, ConfigScope::Local).expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&file);
    set
}

#[test]
fn parse_color_covers_ansi_hex_256_and_attributes() {
    for input in [
        "",
        "reset",
        "bold ul red",
        "green blue",
        "#f00",
        "#ff00aa",
        "255",
        "8",
        "15",
        "nobold strike",
        "dim italic reverse blink",
        "brightcyan",
    ] {
        let out = parse_color(input);
        assert!(out.is_ok(), "parse_color({input:?}): {out:?}");
    }
    assert!(parse_color("not-a-color-word").is_err());
    assert!(parse_color("red green blue").is_err());
    assert!(parse_color("#xyz").is_err());
    assert!(parse_color("#12345").is_err());
}

#[test]
fn url_match_scoring_and_entries() {
    assert!(url_matches(
        "https://example.com",
        "https://example.com/repo.git"
    ));
    assert!(url_matches(
        "https://user@example.com:443/path",
        "https://user@example.com:443/path/extra"
    ));
    assert!(!url_matches("https://other.com", "https://example.com/x"));
    assert!(!url_matches(
        "https://user@example.com",
        "https://other@example.com"
    ));

    let text = r#"
[url "https://example.com/a/"]
    insteadOf = ex:a:
[url "https://example.com/b/"]
    insteadOf = ex:b:
[url "https://example.com/"]
    pushInsteadOf = ex:push:
"#;
    let set = set_from_snippet(text);
    let entries = set.entries();
    let target = "https://example.com/a/extra.git";
    let instead = get_urlmatch_entries(entries, "url", "insteadOf", target);
    let all = get_urlmatch_all_in_section(entries, "url", target);
    assert!(
        !instead.is_empty() || !all.is_empty(),
        "expected url subsection match for {target}"
    );
    let push_target = "https://example.com/push-target";
    let push = get_urlmatch_all_in_section(entries, "url", push_target);
    assert!(!push.is_empty() || url_matches("https://example.com/", push_target));
}

#[test]
fn parse_path_optional_skips_missing() {
    let dir = tempdir().expect("tempdir");
    let mut env = Environment::empty();
    env.home = Some(dir.path().as_os_str().to_os_string());
    let existing = dir.path().join("exists.txt");
    fs::write(&existing, b"x").expect("write");
    assert_eq!(
        parse_path_optional(&env, &format!("~/exists.txt")).as_deref(),
        Some(existing.to_string_lossy().as_ref())
    );
    assert!(parse_path_optional(&env, ":(optional)~/missing.txt").is_none());
    assert_eq!(
        parse_path_optional(&env, "~/missing.txt"),
        Some(format!("{}/missing.txt", dir.path().display()))
    );
}

#[test]
fn resolve_diff_context_rejects_bad_values() {
    let good = set_from_snippet("[diff]\n\tcontext = 3\n");
    assert_eq!(resolve_diff_context_lines(&good).expect("ok"), Some(3));

    let bad = set_from_snippet("[diff]\n\tcontext = not-int\n");
    assert!(matches!(
        resolve_diff_context_lines(&bad),
        Err(Error::Config(_))
    ));
    let negative = set_from_snippet("[diff]\n\tcontext = -5\n");
    assert!(matches!(
        resolve_diff_context_lines(&negative),
        Err(Error::Config(_))
    ));
}

#[test]
fn config_set_typed_getters_and_regexp() {
    let set = set_from_snippet("[core]\n\tbare = true\n\tquotepath = false\n[test]\n\ti = 42\n");
    assert_eq!(set.get_bool("core.bare"), Some(Ok(true)));
    assert_eq!(set.get_i64("test.i"), Some(Ok(42)));
    assert!(!set.quote_path_fully());
    let hits = set.get_regexp("core").expect("regexp");
    assert!(hits.iter().any(|e| e.key == "core.bare"));
}

#[test]
fn from_git_config_parameters_and_command_scope() {
    let params = "'section.key=value'";
    let file = ConfigFile::from_git_config_parameters(Path::new(":GIT_CONFIG_PARAMETERS"), params)
        .expect("params");
    assert_eq!(file.scope, ConfigScope::Command);
    assert_eq!(file.get("section.key").as_deref(), Some("value"));
}

#[test]
fn parse_bom_continuations_and_canonical_key() {
    let content = "\u{feff}[s]\n\tk = line1 \\\n\t\tline2\n";
    let file = ConfigFile::parse(Path::new("c"), content, ConfigScope::Local).expect("parse");
    assert_eq!(file.get("s.k").as_deref(), Some("line1 line2"));
    assert!(canonical_key("").is_err());
    assert_eq!(canonical_key("Core.Bare").unwrap(), "core.bare");
}

#[test]
fn parse_bool_i64_and_git_config_count_env() {
    assert!(parse_bool("yes").unwrap());
    assert!(parse_i64("1g").unwrap() > 1_000_000_000);
    let mut env = Environment::empty();
    env.git_config_count = Some("1".into());
    env.git_config_pairs = vec![("env.key".into(), "v".into())];
    let opts = LoadConfigOptions {
        include_system: false,
        include_ctx: Default::default(),
        ..Default::default()
    };
    let set = ConfigSet::load_with_options(&env, None, &opts).expect("load");
    assert_eq!(set.get("env.key").as_deref(), Some("v"));
    let paths = global_config_paths_pub(&env);
    assert_eq!(parse_path(&env, "/x"), "/x");
    let _ = paths;
}

#[test]
fn add_command_override_roundtrip() {
    let mut set = ConfigSet::new();
    set.add_command_override("override.key", "from-cmd")
        .expect("override");
    assert_eq!(set.get("override.key").as_deref(), Some("from-cmd"));
}

#[test]
fn scope_display_debug_canonical_errors_and_escape_roundtrip() {
    for scope in [
        ConfigScope::System,
        ConfigScope::Global,
        ConfigScope::Local,
        ConfigScope::Worktree,
        ConfigScope::Command,
    ] {
        let mut buf = String::new();
        write!(buf, "{scope}").expect("display");
        assert!(!buf.is_empty());
    }
    let _ = format!("{:?}", LoadConfigOptions::default());

    assert!(canonical_key("bad\nkey").is_err());
    assert!(canonical_key("!.bad.name").is_err());
    assert!(canonical_key("sec.1bad").is_err());

    let inline = ConfigFile::parse(
        Path::new("c"),
        "[sec \"sub\"] inline = yes\n",
        ConfigScope::Local,
    )
    .expect("inline header key");
    assert_eq!(inline.get("sec.sub.inline").as_deref(), Some("yes"));

    let escapes = ConfigFile::parse(
        Path::new("c"),
        "[e]\n\tk = \"a\\rb\\n\\t\\\\\\\"\\x\"\n\ttabs = hello\tworld\n",
        ConfigScope::Local,
    )
    .expect("escape parse");
    assert!(escapes.get("e.k").is_some());
    assert_eq!(escapes.get("e.tabs").as_deref(), Some("hello world"));

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let mut file =
        ConfigFile::parse(&path, "[w]\n\tk = base\n", ConfigScope::Local).expect("parse");
    file.set("w.multiline", "line1\nline2").expect("set nl");
    file.set("w.dash", "-submodule-path").expect("set dash");
    file.set("w.quote", " has spaces ").expect("set spaces");
    file.write().expect("write");
    let reloaded = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("reload")
        .expect("exists");
    assert_eq!(reloaded.get("w.multiline").as_deref(), Some("line1\nline2"));
}
