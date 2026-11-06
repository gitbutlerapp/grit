//! Typed config and submodule errors: each failure is a specific variant, and the system `git`
//! rejects the same input where it has an equivalent check.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::Environment;
use grit_lib::config::gitmodules::gitdir::{
    submodule_gitdir_filesystem_path, validate_legacy_submodule_git_dir, validate_submodule_path,
};
use grit_lib::config::{
    canonical_key, global_config_write_path, ConfigFile, ConfigIncludeOrigin, ConfigLocation,
    ConfigScope, ConfigSet, IncludeContext,
};
use grit_lib::error::{BadNumericSource, ConfigError, Error, InvalidKeyReason, SubmoduleError};
use grit_test_support::git_cmd;
use tempfile::tempdir;

fn config_error(err: Error) -> ConfigError {
    match err {
        Error::Config(e) => e,
        other => panic!("expected a config error, got {other:?}"),
    }
}

fn submodule_error(err: Error) -> SubmoduleError {
    match err {
        Error::Submodule(e) => e,
        other => panic!("expected a submodule error, got {other:?}"),
    }
}

fn env_with_cwd(cwd: &Path) -> Environment {
    let mut env = Environment::empty();
    env.cwd = cwd.to_path_buf();
    env
}

#[test]
fn canonical_key_reports_which_part_is_malformed() {
    let cases = [
        ("core.bare\nx", InvalidKeyReason::LineBreak),
        ("nosection", InvalidKeyReason::MissingSection),
        ("core.", InvalidKeyReason::MissingVariable),
        ("bad_sec.key", InvalidKeyReason::InvalidSection),
        (".key", InvalidKeyReason::InvalidSection),
        ("core.1bare", InvalidKeyReason::InvalidVariable),
        ("core.ba_re", InvalidKeyReason::InvalidVariable),
    ];
    for (key, reason) in cases {
        let err = config_error(canonical_key(key).expect_err(key));
        assert_eq!(
            err,
            ConfigError::InvalidKey {
                key: key.to_owned(),
                reason,
            },
            "{key:?}"
        );
    }
    assert_eq!(canonical_key("Core.Bare").expect("valid"), "core.bare");
}

#[test]
fn invalid_key_messages_name_the_key_without_raw_line_breaks() {
    let err = config_error(canonical_key("a.b\nc").expect_err("newline"));
    assert_eq!(err.to_string(), "invalid key: 'a.b\\nc'");
    let err = config_error(canonical_key("core.").expect_err("no variable"));
    assert!(err.to_string().contains("'core.'"), "{err}");
}

#[test]
fn system_git_rejects_the_same_malformed_keys() {
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("cfg");
    fs::write(&cfg, "").expect("write cfg");
    let cfg = cfg.to_str().expect("utf8 path");
    for key in ["nosection", "core.", "bad_sec.key", "core.1bare"] {
        assert!(canonical_key(key).is_err(), "grit accepts {key:?}");
        let out = git_cmd(&["config", "--file", cfg, key, "v"])
            .in_dir(dir.path())
            .exec();
        assert!(!out.ok(), "git accepts {key:?}");
    }
}

#[test]
fn invalid_value_pattern_keeps_the_pattern() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let mut file =
        ConfigFile::parse(&path, "[a]\n\tb = 1\n", ConfigScope::Local).expect("parse config");
    let err = config_error(
        file.unset_matching("a.b", Some("!(unclosed"), false)
            .expect_err("bad regex"),
    );
    match err {
        ConfigError::InvalidValuePattern { pattern, reason } => {
            assert_eq!(pattern, "!(unclosed");
            assert!(!reason.is_empty());
        }
        other => panic!("expected InvalidValuePattern, got {other:?}"),
    }
}

#[test]
fn rename_section_rejects_invalid_section_names() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let mut file =
        ConfigFile::parse(&path, "[a]\n\tb = 1\n", ConfigScope::Local).expect("parse config");
    for new_name in ["bad_name", "remote.", "", "bad_sec.sub"] {
        let err = config_error(file.rename_section("a", new_name).expect_err(new_name));
        assert_eq!(
            err,
            ConfigError::InvalidSectionName {
                section: new_name.to_owned()
            },
            "{new_name:?}"
        );
    }
    assert!(Error::Config(ConfigError::RepositoryRequired).wants_fatal_prefix());
    assert!(
        Error::Submodule(SubmoduleError::EmptyPath).wants_fatal_prefix(),
        "submodule errors keep the fatal: prefix they had as config errors"
    );
}

#[test]
fn entries_with_prefix_rejects_empty_and_malformed_prefixes() {
    let set = ConfigSet::new();
    for prefix in ["", "   ", ".", "bad_sec.", "-x_y"] {
        match set.entries_with_prefix(prefix) {
            Err(Error::Config(ConfigError::InvalidListPrefix { prefix: got })) => {
                assert_eq!(got, prefix);
            }
            other => panic!("{prefix:?}: expected InvalidListPrefix, got {other:?}"),
        }
    }
    assert!(set.entries_with_prefix("remote.origin").is_ok());
}

#[test]
fn compression_levels_use_bad_numeric_value_with_the_key() {
    let mut set = ConfigSet::new();
    set.add_command_override("core.compression", "10")
        .expect("override");
    let err = config_error(set.loose_objects_zlib_level().expect_err("level 10"));
    assert_eq!(
        err,
        ConfigError::BadNumericValue {
            key: "core.compression".to_owned(),
            value: "10".to_owned(),
            reason: BadNumericSource::OutOfRange,
        }
    );

    let mut set = ConfigSet::new();
    set.add_command_override("pack.compression", "fast")
        .expect("override");
    let err = config_error(set.pack_objects_zlib_level().expect_err("not a number"));
    assert_eq!(
        err,
        ConfigError::BadNumericValue {
            key: "pack.compression".to_owned(),
            value: "fast".to_owned(),
            reason: BadNumericSource::InvalidUnit,
        }
    );
}

#[test]
fn environment_config_errors_are_typed_and_git_rejects_them_too() {
    let dir = tempdir().expect("tempdir");
    let cases: [(&str, &[(&str, &str)], ConfigError); 4] = [
        (
            "bogus count",
            &[("GIT_CONFIG_COUNT", "abc")],
            ConfigError::InvalidEnvironmentCount {
                value: "abc".to_owned(),
            },
        ),
        (
            "count above i32::MAX",
            &[("GIT_CONFIG_COUNT", "4294967296")],
            ConfigError::InvalidEnvironmentCount {
                value: "4294967296".to_owned(),
            },
        ),
        (
            "missing key",
            &[("GIT_CONFIG_COUNT", "1")],
            ConfigError::MissingEnvironmentKey { index: 0 },
        ),
        (
            "malformed parameters",
            &[("GIT_CONFIG_PARAMETERS", "'core.bare'x")],
            ConfigError::MalformedConfigParameters,
        ),
    ];
    for (label, vars, expected) in cases {
        let mut env = env_with_cwd(dir.path());
        for &(name, value) in vars {
            match name {
                "GIT_CONFIG_COUNT" => env.git_config_count = Some(value.to_owned()),
                "GIT_CONFIG_PARAMETERS" => env.git_config_parameters = Some(value.to_owned()),
                _ => unreachable!(),
            }
        }
        let err = config_error(
            ConfigSet::load(
                &env,
                None,
                &crate::config::LoadConfigOptions::standard(&env, None),
            )
            .expect_err(label),
        );
        assert_eq!(err, expected, "{label}");

        let mut cmd = git_cmd(&["config", "--list"])
            .in_dir(dir.path())
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0");
        for &(name, value) in vars {
            cmd = cmd.env(name, value);
        }
        assert!(!cmd.exec().ok(), "git accepts {label}");
    }
}

#[test]
fn relative_include_from_non_file_config_is_typed_or_skipped() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("inc"), "[x]\n\ty = 1\n").expect("write include");
    let blob = ConfigFile::parse_with_origin(
        &dir.path().join("blob"),
        "[include]\n\tpath = inc\n",
        ConfigScope::Local,
        ConfigIncludeOrigin::Blob,
    )
    .expect("parse blob config");
    let mut set = ConfigSet::new();
    let err = config_error(
        set.merge_file_with_includes(&blob, true, &IncludeContext::default())
            .expect_err("relative include from a blob"),
    );
    assert_eq!(err, ConfigError::RelativeIncludeOutsideFile);

    let command_line = ConfigFile::parse_with_origin(
        &dir.path().join("cmdline"),
        "[include]\n\tpath = inc\n",
        ConfigScope::Command,
        ConfigIncludeOrigin::CommandLine,
    )
    .expect("parse command-line config");
    let mut set = ConfigSet::new();
    set.merge_file_with_includes(&command_line, true, &IncludeContext::default())
        .expect("relative command-line include is skipped by default");
    assert_eq!(set.get("x.y").unwrap(), None);

    let strict = IncludeContext {
        command_line_relative_include_is_error: true,
        ..IncludeContext::default()
    };
    let err = config_error(
        ConfigSet::new()
            .merge_file_with_includes(&command_line, true, &strict)
            .expect_err("strict command-line include"),
    );
    assert_eq!(err, ConfigError::RelativeIncludeOutsideFile);
}

#[test]
fn repository_locations_require_a_git_dir() {
    let dir = tempdir().expect("tempdir");
    let env = env_with_cwd(dir.path());
    for location in [ConfigLocation::Local, ConfigLocation::Worktree] {
        let err = config_error(location.write_path(&env, None).expect_err("no git dir"));
        assert_eq!(err, ConfigError::RepositoryRequired, "{location:?}");
    }
}

#[test]
fn global_write_path_without_home_is_typed() {
    let dir = tempdir().expect("tempdir");
    let env = env_with_cwd(dir.path());
    let err = config_error(global_config_write_path(&env).expect_err("no home"));
    assert_eq!(err, ConfigError::NoGlobalConfigPath);

    let mut env = env_with_cwd(dir.path());
    env.home = Some(dir.path().as_os_str().to_owned());
    assert_eq!(
        global_config_write_path(&env).expect("home set"),
        dir.path().join(".gitconfig")
    );
    let xdg = dir.path().join(".config/git/config");
    fs::create_dir_all(xdg.parent().expect("xdg parent")).expect("mkdir xdg");
    fs::write(&xdg, "").expect("write xdg");
    assert_eq!(global_config_write_path(&env).expect("xdg exists"), xdg);
    fs::write(dir.path().join(".gitconfig"), "").expect("write .gitconfig");
    assert_eq!(
        global_config_write_path(&env).expect(".gitconfig exists"),
        dir.path().join(".gitconfig")
    );
}

#[test]
fn submodule_path_validation_is_typed() {
    let dir = tempdir().expect("tempdir");
    assert_eq!(
        submodule_error(validate_submodule_path(dir.path(), "").expect_err("empty")),
        SubmoduleError::EmptyPath
    );
    #[cfg(unix)]
    {
        fs::create_dir(dir.path().join("real")).expect("mkdir");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link"))
            .expect("symlink");
        assert_eq!(
            submodule_error(validate_submodule_path(dir.path(), "link/sub").expect_err("link")),
            SubmoduleError::SymlinkInPath {
                path: "link/sub".to_owned(),
                component: "link".to_owned(),
            }
        );
    }
    validate_submodule_path(dir.path(), "plain/sub").expect("plain path");
}

#[test]
fn legacy_submodule_git_dir_must_end_with_the_name() {
    let dir = tempdir().expect("tempdir");
    let modules = dir.path().join(".git/modules");
    validate_legacy_submodule_git_dir(&modules.join("lib"), "lib").expect("matching name");
    for (git_dir, name) in [
        (modules.join("lib"), "other"),
        (modules.join("xlib"), "lib"),
        (Path::new("lib").to_path_buf(), "lib"),
    ] {
        assert_eq!(
            submodule_error(
                validate_legacy_submodule_git_dir(&git_dir, name).expect_err("name mismatch")
            ),
            SubmoduleError::GitDirNameMismatch {
                name: name.to_owned(),
                git_dir,
            }
        );
    }
}

#[test]
fn missing_configured_submodule_git_dir_is_typed() {
    let dir = tempdir().expect("tempdir");
    let git_dir = dir.path().join(".git");
    fs::create_dir_all(&git_dir).expect("mkdir .git");
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tsubmodulePathConfig = true\n",
    )
    .expect("write config");
    let cfg = ConfigFile::parse(&git_dir.join("config"), "", ConfigScope::Local).expect("parse");
    let err = submodule_error(
        submodule_gitdir_filesystem_path(dir.path(), &git_dir, &cfg, "lib")
            .expect_err("gitdir not set"),
    );
    assert_eq!(
        err,
        SubmoduleError::GitDirNotConfigured {
            name: "lib".to_owned()
        }
    );
    assert_eq!(Error::Submodule(err).kind_name(), "submodule");
}
