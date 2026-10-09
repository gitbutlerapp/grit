//! Config file writing oracle tests (Git t1300/t1303 write subset).
//!
//! For each case, start from the same initial file in two temp copies; apply the
//! equivalent edit with `git config --file` on copy A and [`ConfigFile`] APIs on
//! copy B, then assert byte-identical results unless documented otherwise.

use grit_lib::config::{config_lock_path, ConfigFile, ConfigScope, ConfigSet};
use grit_lib::error::{ConfigError, Error};
use grit_test_support::{git_cmd, Output as GitOutput};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[derive(Clone, Copy, Debug)]
enum Op {
    Set(&'static str, &'static str),
    SetWithComment(&'static str, &'static str, &'static str),
    AddValue(&'static str, &'static str),
    AddValueWithComment(&'static str, &'static str, &'static str),
    ReplaceAll(&'static str, &'static str),
    ReplaceAllPattern(&'static str, &'static str, &'static str),
    ReplaceAllWithComment(&'static str, &'static str, &'static str),
    UnsetLast(&'static str),
    Unset(&'static str),
    UnsetMatching(&'static str, Option<&'static str>),
    CountExpect(&'static str, usize),
    RemoveSection(&'static str),
    RenameSection(&'static str, &'static str),
}

fn git_config(path: &Path, args: &[&str]) -> GitOutput {
    let file = path.to_str().expect("utf-8 path");
    let mut full: Vec<&str> = vec!["config", "--file", file];
    full.extend(args);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    git_cmd(&full).in_dir(dir).exec()
}

fn git_config_ok(path: &Path, args: &[&str]) {
    let out = git_config(path, args);
    assert!(
        out.ok(),
        "git config failed: status={:?} stderr={}",
        out.status,
        out.stderr
    );
}

fn load_grit(path: &Path, text: &str) -> ConfigFile {
    ConfigFile::parse(path, text, ConfigScope::Local).expect("parse initial")
}

fn apply_grit(cfg: &mut ConfigFile, op: Op) -> Result<(), Error> {
    match op {
        Op::Set(k, v) => cfg.set(k, v),
        Op::SetWithComment(k, v, c) => cfg.set_with_comment(k, v, Some(c)),
        Op::AddValue(k, v) => cfg.add_value(k, v),
        Op::AddValueWithComment(k, v, c) => cfg.add_value_with_comment(k, v, Some(c)),
        Op::ReplaceAll(k, v) => cfg.replace_all(k, v, None),
        Op::ReplaceAllPattern(k, v, pat) => cfg.replace_all(k, v, Some(pat)),
        Op::ReplaceAllWithComment(k, v, c) => cfg.replace_all_with_comment(k, v, None, Some(c)),
        Op::UnsetLast(k) => {
            cfg.unset_last(k)?;
            Ok(())
        }
        Op::Unset(k) => {
            cfg.unset(k)?;
            Ok(())
        }
        Op::UnsetMatching(k, pat) => {
            cfg.unset_matching(k, pat, false)?;
            Ok(())
        }
        Op::CountExpect(k, n) => {
            assert_eq!(cfg.count(k)?, n);
            Ok(())
        }
        Op::RemoveSection(s) => {
            cfg.remove_section(s)?;
            Ok(())
        }
        Op::RenameSection(old, new) => {
            cfg.rename_section(old, new)?;
            Ok(())
        }
    }
}

fn apply_git(path: &Path, op: Op) {
    match op {
        Op::Set(k, v) => git_config_ok(path, &[k, v]),
        Op::SetWithComment(k, v, _c) => {
            // Git 2.43 on CI lacks `--comment`; compare via grit-only fixture below.
            git_config_ok(path, &[k, v]);
        }
        Op::AddValue(k, v) => git_config_ok(path, &["--add", k, v]),
        Op::AddValueWithComment(k, v, _c) => git_config_ok(path, &["--add", k, v]),
        Op::ReplaceAll(k, v) => git_config_ok(path, &["--replace-all", k, v]),
        Op::ReplaceAllPattern(k, v, pat) => git_config_ok(path, &["--replace-all", k, v, pat]),
        Op::ReplaceAllWithComment(k, v, _c) => {
            git_config_ok(path, &["--replace-all", k, v]);
        }
        Op::UnsetLast(k) => git_config_ok(path, &["--unset", k]),
        Op::Unset(k) => git_config_ok(path, &["--unset-all", k]),
        Op::UnsetMatching(k, Some(pat)) => git_config_ok(path, &["--unset", k, pat]),
        Op::UnsetMatching(k, None) => git_config_ok(path, &["--unset-all", k]),
        Op::CountExpect(k, n) => {
            let out = git_config(path, &["--get-all", k]);
            assert!(out.ok());
            let stdout = out.stdout.clone();
            let lines: Vec<_> = stdout.lines().filter(|l| !l.is_empty()).collect();
            assert_eq!(lines.len(), n, "git get-all count for {k}");
        }
        Op::RemoveSection(s) => git_config_ok(path, &["--remove-section", s]),
        Op::RenameSection(old, new) => git_config_ok(path, &["--rename-section", old, new]),
    }
}

fn oracle_byte_match(initial: &str, op: Op) {
    let dir = tempdir().expect("tempdir");
    let git_path = dir.path().join("git.cfg");
    let grit_path = dir.path().join("grit.cfg");
    fs::write(&git_path, initial).expect("write git copy");
    fs::write(&grit_path, initial).expect("write grit copy");

    apply_git(&git_path, op);
    let mut cfg = load_grit(&grit_path, initial);
    apply_grit(&mut cfg, op).expect("grit apply");
    cfg.write().expect("grit write");

    let git_bytes = fs::read(&git_path).expect("read git");
    let grit_bytes = fs::read(&grit_path).expect("read grit");
    assert_eq!(
        git_bytes,
        grit_bytes,
        "byte mismatch for {:?}\ngit:\n{}\ngrit:\n{}",
        op,
        String::from_utf8_lossy(&git_bytes),
        String::from_utf8_lossy(&grit_bytes)
    );
}

struct Case {
    name: &'static str,
    initial: &'static str,
    op: Op,
    /// When true, skip git (e.g. comment suffix not on system git).
    grit_only: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "set_new_key_new_section",
        initial: "",
        op: Op::Set("user.email", "a@example.com"),
        grit_only: false,
    },
    Case {
        name: "set_existing_key",
        initial: "[user]\n\tname = Ada\n",
        op: Op::Set("user.name", "Grace"),
        grit_only: false,
    },
    Case {
        name: "set_new_key_existing_section",
        initial: "[core]\n\tbare = false\n",
        op: Op::Set("core.filemode", "true"),
        grit_only: false,
    },
    Case {
        name: "set_duplicate_section_last",
        initial: "[core]\n\ta = 1\n[core]\n\tb = 2\n",
        op: Op::Set("core.d", "4"),
        grit_only: false,
    },
    Case {
        name: "set_updates_last_occurrence_in_last_section",
        initial: "[core]\n\ta = 1\n[core]\n\tb = 2\n",
        op: Op::Set("core.b", "9"),
        grit_only: false,
    },
    Case {
        name: "replace_all_duplicate_key_across_sections",
        initial: "[core]\n\ta = 1\n[core]\n\ta = 2\n",
        op: Op::ReplaceAll("core.a", "9"),
        grit_only: false,
    },
    Case {
        name: "subsection_quotes_and_escapes",
        initial: "",
        op: Op::Set("remote.origin.url", "https://example.com/repo.git"),
        grit_only: false,
    },
    Case {
        name: "value_leading_trailing_spaces",
        initial: "",
        op: Op::Set("note.text", "  padded  "),
        grit_only: false,
    },
    Case {
        name: "value_hash_and_semicolon",
        initial: "",
        op: Op::Set("alias.st", "status # quick"),
        grit_only: false,
    },
    Case {
        name: "value_newline",
        initial: "",
        op: Op::Set("foo.bar", "line1\nline2"),
        grit_only: false,
    },
    Case {
        name: "subsection_backslash_in_name",
        initial: "",
        op: Op::Set(r#"remote."weird\"name".url"#, "u"),
        grit_only: false,
    },
    Case {
        name: "add_value_multivar",
        initial: "[remote \"origin\"]\n\turl = a\n",
        op: Op::AddValue("remote.origin.url", "b"),
        grit_only: false,
    },
    Case {
        name: "replace_all_multivar",
        initial: "[remote \"origin\"]\n\turl = a\n\turl = b\n",
        op: Op::ReplaceAll("remote.origin.url", "c"),
        grit_only: false,
    },
    Case {
        name: "replace_all_value_pattern",
        initial: "[remote \"origin\"]\n\turl = keep\n\turl = drop\n",
        op: Op::ReplaceAllPattern("remote.origin.url", "new", "drop"),
        grit_only: false,
    },
    Case {
        name: "replace_all_negated_pattern",
        initial: "[remote \"origin\"]\n\turl = keep\n\turl = drop\n",
        op: Op::ReplaceAllPattern("remote.origin.url", "new", "!keep"),
        grit_only: false,
    },
    Case {
        name: "unset_last_single",
        initial: "[user]\n\tname = Ada\n",
        op: Op::UnsetLast("user.name"),
        grit_only: false,
    },
    Case {
        name: "unset_all_multivar",
        initial: "[remote \"origin\"]\n\turl = a\n\turl = b\n",
        op: Op::Unset("remote.origin.url"),
        grit_only: false,
    },
    Case {
        name: "unset_matching_value",
        initial: "[remote \"origin\"]\n\turl = a\n\turl = b\n",
        op: Op::UnsetMatching("remote.origin.url", Some("b")),
        grit_only: false,
    },
    Case {
        name: "count_multivar",
        initial: "[remote \"origin\"]\n\turl = a\n\turl = b\n",
        op: Op::CountExpect("remote.origin.url", 2),
        grit_only: false,
    },
    Case {
        name: "remove_section_strips_body",
        initial: "# header\n[core]\n\t# inner\n\tbare = false\n# between\n[user]\n\tname = x\n",
        op: Op::RemoveSection("core"),
        grit_only: false,
    },
    Case {
        name: "rename_section",
        initial: "[branch \"main\"]\n\tremote = origin\n",
        op: Op::RenameSection("branch.main", "branch.develop"),
        grit_only: false,
    },
    Case {
        name: "rename_to_subsection",
        initial: "[branch]\n\tremote = o\n",
        op: Op::RenameSection("branch", "branch.main"),
        grit_only: false,
    },
    Case {
        name: "preserve_unrelated_comments_and_casing",
        initial: "# top\n[User]\n\t# keep me\n\tName = Ada\n",
        op: Op::Set("User.Name", "Grace"),
        grit_only: false,
    },
    Case {
        name: "set_with_comment_suffix",
        initial: "[a]\n\tx = 1\n",
        op: Op::SetWithComment("a.x", "2", "note"),
        grit_only: true,
    },
    Case {
        name: "add_value_with_comment",
        initial: "[a]\n\tx = 1\n",
        op: Op::AddValueWithComment("a.y", "2", "# inline"),
        grit_only: true,
    },
    Case {
        name: "replace_all_with_comment",
        initial: "[a]\n\tx = 1\n\tx = 2\n",
        op: Op::ReplaceAllWithComment("a.x", "9", "replaced"),
        grit_only: true,
    },
];

#[test]
fn t1300_edit_matches_git_byte_for_byte() {
    for case in CASES {
        if case.grit_only {
            let dir = tempdir().expect("tempdir");
            let path = dir.path().join("cfg");
            fs::write(&path, case.initial).expect("write");
            let mut cfg = load_grit(&path, case.initial);
            apply_grit(&mut cfg, case.op).expect("apply");
            cfg.write().expect("write");
            let bytes = fs::read(&path).expect("read");
            let cfg2 = ConfigFile::from_path(&path, ConfigScope::Local)
                .expect("reload")
                .expect("exists");
            let set = {
                let mut s = ConfigSet::new();
                s.merge(&cfg2);
                s
            };
            match case.op {
                Op::SetWithComment(k, v, _) | Op::ReplaceAllWithComment(k, v, _) => {
                    assert_eq!(set.get(k).as_deref(), Some(v));
                }
                Op::AddValueWithComment(k, v, _) => {
                    assert!(set.get_all(k).iter().any(|x| x == v));
                }
                _ => {}
            }
            assert!(
                !bytes.is_empty() || case.initial.is_empty(),
                "{} produced empty unexpectedly",
                case.name
            );
            continue;
        }
        oracle_byte_match(case.initial, case.op);
    }
}

#[test]
fn multivar_set_and_unset_last_are_typed_errors() {
    let initial = "[remote \"origin\"]\n\turl = a\n\turl = b\n";
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    fs::write(&path, initial).unwrap();
    let mut cfg = load_grit(&path, initial);

    let err = cfg.set("remote.origin.url", "c").unwrap_err();
    assert!(matches!(
        err,
        Error::Config(ConfigError::MultipleValues { .. })
    ));

    let err = cfg.unset_last("remote.origin.url").unwrap_err();
    assert!(matches!(
        err,
        Error::Config(ConfigError::MultipleValues { .. })
    ));
}

#[test]
fn rename_section_invalid_name_is_typed_error() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    fs::write(&path, "[a]\n\tx = 1\n").unwrap();
    let mut cfg = load_grit(&path, "[a]\n\tx = 1\n");
    let err = cfg.rename_section("a", "bad name").unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn config_lock_present_is_typed_error_and_preserves_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config");
    let before = b"[user]\n\tname = Ada\n";
    fs::write(&path, before).unwrap();
    fs::write(config_lock_path(&path), b"held").unwrap();

    let cfg = load_grit(&path, std::str::from_utf8(before).unwrap());
    let err = cfg.write().unwrap_err();
    assert!(matches!(
        err,
        Error::Config(ConfigError::ConfigFileLocked { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(config_lock_path(&path).exists());
    assert!(fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .all(|p| !p.file_name().unwrap().to_string_lossy().ends_with(".tmp")));
}

#[test]
fn write_is_atomic_no_stray_lock_after_success() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config");
    fs::write(&path, "[a]\n\tx = 1\n").unwrap();
    let mut cfg = load_grit(&path, "[a]\n\tx = 1\n");
    cfg.set("a.x", "2").unwrap();
    cfg.write().unwrap();
    assert!(!config_lock_path(&path).exists());
    assert_eq!(fs::read_to_string(&path).unwrap(), "[a]\n\tx = 2\n");
}

#[test]
fn grit_edited_repo_config_git_status_and_reread() {
    let dir = tempdir().unwrap();
    git_cmd(&["init"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .in_dir(dir.path())
        .suc();
    let cfg_path = dir.path().join(".git").join("config");
    let mut cfg = ConfigFile::from_path(&cfg_path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    cfg.set("user.email", "writer@example.com").unwrap();
    cfg.write().unwrap();

    git_cmd(&["config", "user.email"])
        .in_dir(dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .suc();

    git_cmd(&["status"])
        .in_dir(dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .suc();

    let reread = ConfigFile::from_path(&cfg_path, ConfigScope::Local)
        .expect("reread")
        .expect("exists");
    let mut set = ConfigSet::new();
    set.merge(&reread);
    assert_eq!(set.get("user.email").as_deref(), Some("writer@example.com"));
}
