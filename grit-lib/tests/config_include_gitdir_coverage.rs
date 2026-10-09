//! `includeIf gitdir:` relative patterns and strict `GIT_CONFIG_PARAMETERS` parsing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::Path;

use grit_lib::config::{
    parse_config_parameters, ConfigFile, ConfigScope, ConfigSet, IncludeContext,
};
use grit_lib::environment::Environment;
use grit_lib::error::Error;
use grit_lib::repo::init_repository;
use tempfile::tempdir;

#[test]
fn relative_gitdir_include_from_disk_file() {
    let dir = tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    init_repository(&repo, false, "main", None, "files").expect("init");
    let git_dir = repo.join(".git");
    let inc = git_dir.join("extra.conf");
    fs::write(&inc, "[hit]\n\tk = yes\n").expect("inc");
    let pattern = format!("gitdir:{}/", repo.canonicalize().expect("canon").display());
    fs::write(
        git_dir.join("config"),
        format!("[includeIf \"{pattern}\"]\n\tpath = extra.conf\n"),
    )
    .expect("cfg");

    let env = Environment::empty();
    let ctx = IncludeContext {
        git_dir: Some(git_dir.clone()),
        cwd: repo.clone(),
        env: std::sync::Arc::new(env),
        ..Default::default()
    };
    let file = ConfigFile::from_path(&git_dir.join("config"), ConfigScope::Local)
        .expect("read")
        .expect("exists");
    let mut set = ConfigSet::new();
    set.merge_file_with_includes(&file, true, &ctx)
        .expect("merge includes");
    assert_eq!(set.get("hit.k").as_deref(), Some("yes"));
}

#[test]
fn parse_config_parameters_pair_and_old_style_mix() {
    let raw = "'a.b=c' 'd.e=f' 'flag.only'";
    let entries = parse_config_parameters(raw);
    assert!(entries.len() >= 2);
    assert_eq!(
        grit_lib::config::git_config_parameters_last_value(raw, "a.b").as_deref(),
        Some("c")
    );
    let err = ConfigFile::from_git_config_parameters(
        Path::new(":GIT_CONFIG_PARAMETERS"),
        "'k=v' badtoken",
    );
    assert!(err.is_err());
}

#[test]
fn config_file_set_creates_new_section_header() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("cfg");
    fs::write(&path, "[existing]\n\tk = 1\n").expect("write");
    let mut file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    file.set("newsect.sub.key", "v").expect("set new section");
    file.write().expect("persist");
    let text = fs::read_to_string(&path).expect("read back");
    assert!(text.contains("[newsect \"sub\"]"));
    assert!(text.contains("key = v"));
}

#[test]
fn inline_section_key_parse_and_negotiation_errors() {
    let ok = ConfigFile::parse(
        Path::new("c"),
        "[sec \"sub\"]\n\tinside = 1\n",
        ConfigScope::Local,
    )
    .expect("inline section value");
    assert_eq!(ok.get("sec.sub.inside").as_deref(), Some("1"));

    let err = ConfigFile::parse(
        Path::new("c"),
        "[fetch]\n\tnegotiationAlgorithm\n",
        ConfigScope::Local,
    );
    assert!(matches!(
        err,
        Err(Error::Message(_)) | Err(Error::Config(_))
    ));

    let inline = ConfigFile::parse(
        Path::new("c"),
        "[remote \"origin\"]\n\turl = https://example.com/r.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        ConfigScope::Local,
    )
    .expect("remote");
    assert!(inline.get("remote.origin.url").is_some());
}
