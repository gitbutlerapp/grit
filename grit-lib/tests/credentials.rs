//! Integration tests for `grit_lib::credentials`.
//!
//! These exercise the Git-compatible [`HelperCredentialProvider`] end-to-end
//! against real on-disk credential helper scripts (shell scripts that speak
//! Git's `key=value` credential protocol on stdin/stdout). A temp repo is
//! created with the system `git`, its `credential.helper` config is pointed at
//! a fake helper, and we assert:
//!
//! - `fill` returns the username/password the helper emits;
//! - `approve`/`reject` invoke the helper with `store`/`erase` (verified by a
//!   helper that records its action to a file);
//! - with **no** helper configured, `fill` returns the typed
//!   non-interactive error (it must not hang on a TTY/askpass).
//!
//! The tests skip gracefully if the system `git` binary is unavailable.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Once;

use grit_lib::config::ConfigSet;
use grit_lib::credentials::{
    Credential, CredentialProvider, HelperCredentialProvider, NON_INTERACTIVE_MESSAGE,
};

/// Run `git` in `dir`; returns `None` if git is unavailable.
fn git(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()
}

/// Initialize a repo in `dir`. Returns `false` if git is unavailable.
fn init_repo(dir: &Path) -> bool {
    matches!(git(dir, &["init", "-q"]), Some(out) if out.status.success())
}

/// Write an executable shell script and return its path.
fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod script");
}

static ISOLATE: Once = Once::new();

/// Detach config loading from the developer's real `~/.gitconfig` and
/// `/etc/gitconfig`. `ConfigSet::load` honors `GIT_CONFIG_GLOBAL` /
/// `GIT_CONFIG_SYSTEM` (pointing them at `/dev/null` yields no entries), so the
/// only `credential.helper` the provider sees is the one the test wrote into
/// the repo-local config. Every test wants the identical isolation, so setting
/// these process-wide once is race-free under parallel test execution.
fn isolate_global_config() {
    ISOLATE.call_once(|| {
        // SAFETY: set once, to a constant, before any test loads config.
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    });
}

/// Load the repo-local config (no global/system) for the worktree at `dir`.
///
/// `ConfigSet::load` wants the `.git` directory, where `config` lives.
fn load_config(dir: &Path) -> ConfigSet {
    isolate_global_config();
    let git_dir = dir.join(".git");
    ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&git_dir),
        false,
    )
    .expect("load config")
}

fn sample_input() -> Credential {
    Credential {
        protocol: Some("https".into()),
        host: Some("example.com".into()),
        ..Default::default()
    }
}

#[test]
fn fill_returns_helper_username_and_password() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("skipping: system git unavailable");
        return;
    }

    // A helper that, on `get`, emits a fixed username/password (and echoes back
    // any input fields it received, like a real helper would not need to).
    let helper = dir.join("helper.sh");
    write_script(
        &helper,
        "#!/bin/sh\n\
         if [ \"$1\" = get ]; then\n\
           echo username=alice\n\
           echo password=secret\n\
         fi\n",
    );

    // Git's `!cmd` shell form runs the value through `sh -c`. Point the helper
    // at the absolute script path.
    let helper_value = format!("!{}", helper.display());
    let cfg = build_config(dir, &[("credential.helper", &helper_value)]);

    let provider = HelperCredentialProvider::new(cfg);
    let filled = provider.fill(&sample_input()).expect("fill should succeed");
    assert_eq!(filled.username.as_deref(), Some("alice"));
    assert_eq!(filled.password.as_deref(), Some("secret"));
    // The provider must preserve the original target fields.
    assert_eq!(filled.protocol.as_deref(), Some("https"));
    assert_eq!(filled.host.as_deref(), Some("example.com"));
}

#[test]
fn approve_and_reject_invoke_helper_with_store_and_erase() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("skipping: system git unavailable");
        return;
    }

    let record = dir.join("record.log");
    let helper = dir.join("recorder.sh");
    // Record the action ($1) so we can assert store/erase were dispatched.
    write_script(
        &helper,
        &format!(
            "#!/bin/sh\n\
             echo \"$1\" >> {record}\n\
             if [ \"$1\" = get ]; then\n\
               echo username=alice\n\
               echo password=secret\n\
             fi\n",
            record = record.display()
        ),
    );

    let helper_value = format!("!{}", helper.display());
    let cfg = build_config(dir, &[("credential.helper", &helper_value)]);
    let provider = HelperCredentialProvider::new(cfg);

    let cred = Credential {
        protocol: Some("https".into()),
        host: Some("example.com".into()),
        username: Some("alice".into()),
        password: Some("secret".into()),
        ..Default::default()
    };

    provider.approve(&cred).expect("approve");
    provider.reject(&cred).expect("reject");

    let log = fs::read_to_string(&record).expect("read record log");
    let actions: Vec<&str> = log.lines().collect();
    assert!(
        actions.contains(&"store"),
        "approve should invoke helper with `store`, got {actions:?}"
    );
    assert!(
        actions.contains(&"erase"),
        "reject should invoke helper with `erase`, got {actions:?}"
    );
}

#[test]
fn fill_without_helper_returns_typed_non_interactive_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("skipping: system git unavailable");
        return;
    }

    // No credential.helper configured.
    let cfg = load_config(dir);
    let provider = HelperCredentialProvider::new(cfg);

    let err = provider
        .fill(&sample_input())
        .expect_err("fill must fail (not hang) with no helper");
    // Must be the typed non-interactive message, not a TTY prompt/hang.
    assert!(
        err.to_string().contains(NON_INTERACTIVE_MESSAGE),
        "expected non-interactive error, got: {err}"
    );
}

#[test]
fn fill_returns_typed_error_when_helper_supplies_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("skipping: system git unavailable");
        return;
    }

    // A helper that returns no credentials at all (mimics e.g. an empty store).
    let helper = dir.join("empty.sh");
    write_script(&helper, "#!/bin/sh\nexit 0\n");
    let helper_value = format!("!{}", helper.display());
    let cfg = build_config(dir, &[("credential.helper", &helper_value)]);

    let provider = HelperCredentialProvider::new(cfg);
    let err = provider
        .fill(&sample_input())
        .expect_err("fill must fail when helper yields nothing");
    assert!(
        err.to_string().contains(NON_INTERACTIVE_MESSAGE),
        "expected non-interactive error, got: {err}"
    );
}

#[test]
fn fill_short_circuits_when_input_already_complete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("skipping: system git unavailable");
        return;
    }

    // Helper that, if ever called, would FAIL the process — proving it is not
    // invoked when the input already has username+password.
    let helper = dir.join("explode.sh");
    write_script(&helper, "#!/bin/sh\nexit 17\n");
    let helper_value = format!("!{}", helper.display());
    let cfg = build_config(dir, &[("credential.helper", &helper_value)]);

    let provider = HelperCredentialProvider::new(cfg);
    let input = Credential {
        protocol: Some("https".into()),
        host: Some("example.com".into()),
        username: Some("bob".into()),
        password: Some("hunter2".into()),
        ..Default::default()
    };
    let filled = provider
        .fill(&input)
        .expect("complete input should not call helper");
    assert_eq!(filled.username.as_deref(), Some("bob"));
    assert_eq!(filled.password.as_deref(), Some("hunter2"));
}

#[test]
fn url_scoped_helper_applies_only_to_matching_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("skipping: system git unavailable");
        return;
    }

    let helper = dir.join("scoped.sh");
    write_script(
        &helper,
        "#!/bin/sh\n\
         if [ \"$1\" = get ]; then\n\
           echo username=scoped\n\
           echo password=pw\n\
         fi\n",
    );
    let helper_value = format!("!{}", helper.display());
    // Scope the helper to github.com only.
    let cfg = build_config(
        dir,
        &[("credential.https://github.com.helper", &helper_value)],
    );
    let provider = HelperCredentialProvider::new(cfg);

    // Matching target -> helper fires, credential completed.
    let github = Credential {
        protocol: Some("https".into()),
        host: Some("github.com".into()),
        ..Default::default()
    };
    let filled = provider.fill(&github).expect("matching scope fills");
    assert_eq!(filled.username.as_deref(), Some("scoped"));

    // Non-matching target -> helper does not apply -> typed non-interactive err.
    let other = Credential {
        protocol: Some("https".into()),
        host: Some("example.com".into()),
        ..Default::default()
    };
    let err = provider
        .fill(&other)
        .expect_err("non-matching scope yields no creds");
    assert!(err.to_string().contains(NON_INTERACTIVE_MESSAGE));
}

#[test]
fn credential_wire_format_round_trips() {
    let input = "protocol=https\nhost=example.com\nusername=alice\npassword=secret\n";
    let cred = Credential::parse(input);
    assert_eq!(cred.username.as_deref(), Some("alice"));
    assert_eq!(cred.password.as_deref(), Some("secret"));
    assert_eq!(cred.serialize(), input);
}

/// Build a [`ConfigSet`] for `dir` with extra `key=value` config applied via
/// `git config --local` (so the entries land in the repo's real `.git/config`),
/// then load it. Keeps the test's config wiring identical to what an embedder
/// would see from a real repository.
fn build_config(dir: &Path, kvs: &[(&str, &str)]) -> ConfigSet {
    for (k, v) in kvs {
        let out = git(dir, &["config", "--local", k, v]).expect("git config");
        assert!(out.status.success(), "git config {k} failed: {out:?}");
    }
    load_config(dir)
}
