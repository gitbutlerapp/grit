//! Issue #2562: local clone must store `remote.origin.url` like `git clone` (Git `absolute_pathdup`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;
use std::string::String;

use grit_lib::config::environment::Environment;
use grit_lib::prelude::ConfigSet;
use grit_lib::progress::CancelToken;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn local_clone_stores_origin_url_matching_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("t");
    let src = root.join("src");
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).expect("mkdir sub");
    git(&root, &["init", "-q", "-b", "main", "src"]);
    git(
        &src,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.test",
            "commit",
            "--allow-empty",
            "-qm",
            "seed",
        ],
    );

    let git_dest = sub.join("git-clone");
    git(&sub, &["clone", "-q", "../src", git_dest.to_str().unwrap()]);
    let git_url = git(&git_dest, &["config", "--get", "remote.origin.url"]);

    let mut env = Environment::empty();
    env.cwd = sub.clone();
    env.git_config_nosystem = Some("1".into());
    env.home = Some(tmp.path().as_os_str().to_owned());
    let grit_dest = sub.join("grit-clone");
    let mut opts = CloneOptions::new(String::from("../src"), grit_dest.clone());
    opts.environment = env;
    clone(opts, &mut NoProgress, &CancelToken::never(), None).expect("grit clone");
    let config = ConfigSet::load(
        &Environment::empty(),
        Some(&grit_dest.join(".git")),
        &grit_lib::config::LoadConfigOptions::standard(&Environment::empty(), None),
    )
    .expect("config");
    let origin = config.get("remote.origin.url").expect("origin url");
    let grit_url = origin.as_deref().expect("value").to_owned();
    assert_eq!(grit_url, git_url, "stored origin URL should match git");
}

#[cfg(unix)]
#[test]
fn local_clone_stored_url_prefers_pwd_prefix() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let physical = tmp.path().join("physical");
    let logical = tmp.path().join("logical");
    std::fs::create_dir_all(&physical).expect("physical");
    symlink(&physical, &logical).expect("symlink");
    let src = physical.join("t/src");
    let sub = physical.join("t/sub");
    std::fs::create_dir_all(&sub).expect("subdir");
    git(&physical.join("t"), &["init", "-q", "-b", "main", "src"]);
    git(
        &src,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.test",
            "commit",
            "--allow-empty",
            "-qm",
            "seed",
        ],
    );

    let logical_sub = logical.join("t/sub");
    let mut env = Environment::empty();
    env.cwd = sub.canonicalize().expect("canon sub");
    env.pwd = Some(logical_sub.to_string_lossy().into_owned());
    env.git_config_nosystem = Some("1".into());
    env.home = Some(tmp.path().as_os_str().to_owned());

    let git_dest = sub.join("git-clone");
    let git_url = std::process::Command::new("git")
        .current_dir(&sub)
        .env("PWD", logical_sub.as_os_str())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["clone", "-q", "../src"])
        .arg(git_dest.to_str().expect("utf8"))
        .output()
        .expect("git clone");
    assert!(
        git_url.status.success(),
        "git clone: {}",
        String::from_utf8_lossy(&git_url.stderr)
    );
    let git_url = std::process::Command::new("git")
        .args([
            "-C",
            git_dest.to_str().expect("utf8"),
            "config",
            "--get",
            "remote.origin.url",
        ])
        .output()
        .expect("git config");
    let git_url = String::from_utf8_lossy(&git_url.stdout).trim().to_owned();

    let grit_dest = sub.join("grit-clone");
    let mut opts = CloneOptions::new(String::from("../src"), grit_dest.clone());
    opts.environment = env;
    clone(opts, &mut NoProgress, &CancelToken::never(), None).expect("grit clone");
    let config = ConfigSet::load(
        &Environment::empty(),
        Some(&grit_dest.join(".git")),
        &grit_lib::config::LoadConfigOptions::standard(&Environment::empty(), None),
    )
    .expect("config");
    let origin = config.get("remote.origin.url").expect("origin url");
    let grit_url = origin.as_deref().expect("value").to_owned();
    assert_eq!(
        grit_url, git_url,
        "PWD-prefixed origin URL should match git"
    );
}
