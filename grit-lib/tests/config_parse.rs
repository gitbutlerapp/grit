//! Config file parsing and ConfigSet precedence vs system `git config` (t1300 subset, t1303, t1308, t1310).

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::{
    canonical_key, git_config_parameters_last_value, parse_bool, parse_color,
    parse_config_parameters, parse_git_config_int_strict, parse_i64, parse_path, ConfigFile,
    ConfigScope, ConfigSet, GitConfigIntStrictError,
};
use grit_lib::environment::Environment;
use support::{
    assert_bad_line_error, assert_canonical_key_err, assert_file_get_matches_git,
    assert_typed_bool_matches, assert_typed_int_matches, git_color, git_file_get, git_file_get_all,
    git_file_get_typed_with_home, git_get_urlmatch, grit_color, grit_file_from_content, grit_get,
    grit_get_all, grit_path_optional, grit_path_value, isolated_env, null_device,
};
use tempfile::tempdir;

fn tab(s: &str) -> String {
    s.replace("XX", "\t")
}

/// Whitespace / comment corpus from upstream `t1300-config.sh` (setup block).
fn t1300_whitespace_config() -> String {
    tab(r#"[section]
XXsolid = rock
XXsparse = big XX blue
XXsparseAndTail = big XX blue $
XXsparseAndTailQuoted = "big XX blue "
XXsparseAndBiggerTail = big XX blue X X
XXsparseAndBiggerTailQuoted = "big XX blue X X"
XXsparseAndBiggerTailQuotedPlus = "big XX blue X X"X $
XXheadAndTail = Xbig blue $
XXheadAndTailQuoted = "Xbig blue "
XXheadAndTailQuotedPlus = "Xbig blue " $
XXannotated = big blueX# to be discarded
XXannotatedQuoted = "big blue"X# to be discarded
"#)
}

#[test]
fn t1300_whitespace_corpus_matches_git() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let content = t1300_whitespace_config();
    std::fs::write(&path, &content).expect("write");
    for key in [
        "section.solid",
        "section.sparse",
        "section.sparseAndTail",
        "section.sparseAndTailQuoted",
        "section.sparseAndBiggerTail",
        "section.sparseAndBiggerTailQuoted",
        "section.headAndTail",
        "section.headAndTailQuoted",
        "section.annotated",
        "section.annotatedQuoted",
    ] {
        assert_file_get_matches_git(&path, &content, key);
    }
}

#[test]
fn unquoted_backslash_t_and_b_escapes_match_git() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let cases: &[(&str, &str)] = &[
        ("[s]\n\tescaped = a\\tb\n", "s.escaped"),
        ("[s]\n\tbackspace = a\\bb\n", "s.backspace"),
    ];
    for (content, key) in cases {
        assert_file_get_matches_git(&path, content, key);
        let git_bytes: Vec<u8> = {
            fs::write(&path, content).expect("write");
            let out = Command::new("git")
                .args([
                    "config",
                    "--file",
                    path.display().to_string().as_str(),
                    "--get",
                    key,
                ])
                .env("GIT_CONFIG_GLOBAL", null_device())
                .env("GIT_CONFIG_SYSTEM", null_device())
                .output()
                .expect("git");
            assert!(out.status.success(), "git get {key}");
            String::from_utf8_lossy(&out.stdout)
                .strip_suffix('\n')
                .unwrap_or("")
                .as_bytes()
                .to_vec()
        };
        let file = grit_file_from_content(&path, content, ConfigScope::Local);
        let grit_val = grit_get(&file, key).expect("grit get");
        assert_eq!(
            grit_val.as_bytes(),
            git_bytes.as_slice(),
            "byte parity for {key}"
        );
    }
}

#[test]
fn t1300_values_match_git_config_get() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");

    let cases: &[(&str, &[&str])] = &[
        ("[section]\n\tsolid = rock\n", &["section.solid"]),
        (
            &tab("[section]\nXXsparse = big XX blue\n"),
            &["section.sparse"],
        ),
        (
            &tab("[section]\nXXsparseAndTail = big XX blue $\n"),
            &["section.sparseAndTail"],
        ),
        ("[section]\n\tbarekey\n", &["section.barekey"]),
        (
            "[section \"SubSec\"]\n\tkey = value\n",
            &["section.SubSec.key"],
        ),
        (
            "[user]\n\temail = alice@example.com\n\tname = Alice\n",
            &["user.email", "user.name"],
        ),
        ("[core]\n\tbare = true\n", &["core.bare"]),
        (
            &tab(r#"[s]
XXv = "hello\nworld"
"#),
            &["s.v"],
        ),
        ("[multi]\n\tval = one\n\tval = two\n", &["multi.val"]),
    ];

    for (content, keys) in cases {
        std::fs::write(&path, content).expect("write");
        for key in *keys {
            assert_file_get_matches_git(&path, content, key);
            let git_all = git_file_get_all(&path, key);
            let file = grit_file_from_content(&path, content, ConfigScope::Local);
            let grit_all = grit_get_all(&file, key);
            assert_eq!(grit_all, git_all, "get-all {key}");
            if git_all.len() <= 1 {
                let git_one = git_file_get(&path, key);
                let grit_one = grit_get(&file, key);
                if git_one.as_deref() == Some("") {
                    assert!(
                        grit_one.as_deref() == Some("true") || grit_one.as_deref() == Some(""),
                        "bare key {key}: git={git_one:?} grit={grit_one:?}"
                    );
                } else {
                    assert_eq!(grit_one, git_one, "get {key}");
                }
            }
        }
    }
}

#[test]
fn t1303_wacky_files_match_git() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("wacky");

    let cases: &[(&str, &str)] = &[
        ("\u{feff}[core]\n\tbare = false\n", "core.bare"),
        ("[core]\r\n\tbare = true\r\n", "core.bare"),
    ];

    for (content, key) in cases {
        assert_file_get_matches_git(&path, content, key);
    }

    let mut long = String::from("[long]\n\tval = ");
    long.push_str(&"x".repeat(8192));
    long.push('\n');
    assert_file_get_matches_git(&path, &long, "long.val");

    let bom_content = "\u{feff}[user]\n\tname = bom\n";
    assert_file_get_matches_git(&path, bom_content, "user.name");
}

#[test]
fn typed_getters_match_git_config_type() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("typed");

    let bool_cases = [
        ("[t]\n\ty = true\n", "t.y"),
        ("[t]\n\ty = false\n", "t.y"),
        ("[t]\n\ty = yes\n", "t.y"),
        ("[t]\n\ty = off\n", "t.y"),
    ];
    for (content, key) in bool_cases {
        assert_typed_bool_matches(&path, content, key);
    }

    let int_cases = [
        ("[n]\n\tx = 42\n", "n.x"),
        ("[n]\n\tx = 1k\n", "n.x"),
        ("[n]\n\tx = 2m\n", "n.x"),
    ];
    for (content, key) in int_cases {
        assert_typed_int_matches(&path, content, key);
    }

    let path_content = "[p]\n\tdir = ~/projects\n";
    std::fs::write(&path, path_content).expect("write");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let mut env = isolated_env(&home);
    env.home = Some(home.as_os_str().to_os_string());
    let git_path = git_file_get_typed_with_home(&path, "p.dir", "path", Some(&home))
        .expect("git path")
        .expect("git path value");
    let grit_path = grit_path_value(&env, "~/projects");
    assert_eq!(grit_path, git_path);

    let color_content = "[c]\n\tfg = red bold\n";
    std::fs::write(&path, color_content).expect("write");
    assert_eq!(
        grit_color("red bold").expect("parse"),
        git_color(&path, "c.fg").expect("git color")
    );
}

#[test]
fn parse_helpers_and_canonical_key() {
    assert_eq!(parse_bool("yes").unwrap(), true);
    assert_eq!(parse_bool("off").unwrap(), false);
    assert_eq!(parse_i64("1k").unwrap(), 1024);
    assert_eq!(parse_git_config_int_strict("10").unwrap(), 10);
    assert!(matches!(
        parse_git_config_int_strict("1x"),
        Err(GitConfigIntStrictError::InvalidUnit)
    ));

    assert_eq!(canonical_key("Core.Bare").unwrap(), "core.bare");
    assert_canonical_key_err("nosection");
    assert_canonical_key_err("section.");
    assert_canonical_key_err("no-dot");

    let env = Environment::empty();
    assert_eq!(grit_path_optional(&env, "").as_deref(), Some(""));
    assert!(!parse_path(&env, "%(prefix)/foo").is_empty());
}

#[test]
fn invalid_lines_are_typed_bad_config_line() {
    assert_bad_line_error("[s]\n\t!!!garbage\n", 2);
    assert_bad_line_error("[s]\n\t1bad = x\n", 2);
    assert_bad_line_error("[s]\n\tkey = \"unclosed\n", 2);
}

#[test]
fn multivar_has_key_and_last_entry() {
    let content = "[r]\n\turl = a\n\turl = b\n";
    let file = grit_file_from_content(Path::new("cfg"), content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    set.merge(&file);
    assert!(set.has_key("r.url"));
    assert_eq!(set.get("r.url").as_deref(), Some("b"));
    assert_eq!(set.get_all("r.url"), vec!["a", "b"]);
    assert_eq!(
        set.get_all_raw("r.url"),
        vec![Some("a".into()), Some("b".into())]
    );
    let last = set.get_last_entry("r.url").expect("entry");
    assert_eq!(last.value.as_deref(), Some("b"));
}

#[test]
fn t1308_config_set_precedence_matches_git() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let global = "[user]\n\tname = Global\n\temail = g@example.com\n";
    let local = "[user]\n\tname = Local\n";
    let repo = support::write_layered_repo(&home, global, local);
    let git_dir = repo.join(".git");

    let git_name = {
        let out = Command::new("git")
            .current_dir(&repo)
            .args(["config", "--get", "user.name"])
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("GIT_CONFIG_SYSTEM", null_device())
            .env("HOME", &home)
            .output()
            .expect("git config");
        assert!(out.status.success(), "git config failed: {:?}", out);
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };

    let env = support::env_with_global(&home, &home.join(".gitconfig"));
    let opts = support::default_load_opts(&git_dir, &env);
    let set = support::load_grit_cascade(&env, Some(&git_dir), &opts);
    assert_eq!(set.get("user.name").as_deref(), Some(git_name.as_str()));

    let mut set2 = set.clone();
    set2.add_command_override("user.email", "cmd@example.com")
        .expect("override");
    assert_eq!(set2.get("user.email").as_deref(), Some("cmd@example.com"));
}

#[test]
fn git_config_parameters_and_command_override() {
    let params = "'core.bare=true' 'user.name=Test'";
    let parsed = parse_config_parameters(params);
    assert_eq!(parsed.len(), 2);
    assert_eq!(
        git_config_parameters_last_value(params, "core.bare").as_deref(),
        Some("true")
    );

    let file = ConfigFile::from_git_config_parameters(Path::new(":GIT_CONFIG_PARAMETERS"), params)
        .expect("from params");
    let mut set = ConfigSet::new();
    set.merge(&file);
    assert_eq!(set.get("core.bare").as_deref(), Some("true"));
    assert_eq!(set.get("user.name").as_deref(), Some("Test"));
}

#[test]
fn t1310_config_default_and_urlmatch() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("url");
    let content = r#"
[url "https://example.com/repo.git"]
	insteadOf = ex:
"#;
    std::fs::write(&path, content).expect("write");
    let git_matches = git_get_urlmatch(&path, "insteadOf", "ex:foo");
    if let Ok(git_vals) = git_matches {
        use grit_lib::config::{get_urlmatch_all_in_section, get_urlmatch_entries, url_matches};
        assert!(url_matches("ex:", "ex:foo"));
        let file = grit_file_from_content(&path, content, ConfigScope::Local);
        let mut set = ConfigSet::new();
        set.merge(&file);
        let entries = set.entries();
        let matched = get_urlmatch_entries(entries, "url", "insteadOf", "ex:foo");
        let all = get_urlmatch_all_in_section(entries, "url", "ex:foo");
        assert!(!matched.is_empty() || !all.is_empty() || git_vals.is_empty());
    }
}

#[test]
fn extra_parse_color_and_int_coverage() {
    assert!(parse_color("bold ul red").is_ok());
    assert!(parse_color("reset").is_ok());
    assert!(parse_i64("1g").is_ok());
    assert!(parse_git_config_int_strict("-1").is_ok());
    let mut set = ConfigSet::new();
    set.add_command_override("test.flag", "true").expect("ov");
    assert!(set.get_bool("test.flag").unwrap().unwrap());
}

#[test]
fn get_regexp_and_escape_roundtrip() {
    let content = "[remote \"origin\"]\n\turl = u\n\tfetch = f1\n\tfetch = f2\n";
    let file = grit_file_from_content(Path::new(".git/config"), content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    set.merge(&file);
    let m = set.get_regexp("remote.origin").expect("regexp");
    let fetches: Vec<_> = m
        .iter()
        .filter(|e| e.key == "remote.origin.fetch")
        .collect();
    assert_eq!(fetches.len(), 2);
}

#[test]
fn config_file_set_roundtrip_preserves_get() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    fs::write(&path, "[s]\n\told = 1\n").expect("write");
    let mut file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    file.set("s.new", "2").expect("set");
    file.write().expect("write");
    let reread = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read2")
        .expect("exists2");
    assert_eq!(reread.get("s.new").as_deref(), Some("2"));
}

#[test]
fn parse_gitmodules_best_effort_keeps_prior_entries() {
    let content = "[submodule \"a\"]\n\tpath = p\n[submodule \"b\"]\n\tpath = \"unclosed\n";
    let (entries, bad) = ConfigFile::parse_gitmodules_best_effort(
        Path::new(".gitmodules"),
        content,
        ConfigScope::Local,
    );
    assert!(matches!(bad, Some(3) | Some(4)));
    assert!(entries.iter().any(|e| e.key == "submodule.a.path"));
}

#[test]
fn load_with_includes_disabled() {
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config");
    std::fs::write(&cfg, "[include]\n\tpath = inc\n").expect("parent");
    std::fs::write(dir.path().join("inc"), "[u]\n\tname = inc\n").expect("inc");
    let content = std::fs::read_to_string(&cfg).expect("read");
    let file = grit_file_from_content(&cfg, &content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    let ctx = grit_lib::config::IncludeContext {
        git_dir: None,
        env: std::sync::Arc::new(env),
        ..Default::default()
    };
    set.merge_file_with_includes(&file, false, &ctx)
        .expect("merge no includes");
    assert!(set.get("u.name").is_none());
    assert!(set.get("include.path").is_some());
}
