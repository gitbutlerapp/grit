//! Regression: repeated push prepare must require fresh object transfer each cycle.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_utils::binary::resolve_binary;
use grit_utils::network_fixture::{ensure_campaign_bare_network_meta, file_url};
use grit_utils::network_suite::{
    assert_push_target_lacks_client_tip, configure_remote, ensure_network_client_repo,
    git_object_exists, prepare_network_push,
};
use grit_utils::synth::{generate_synth_repo, SynthTier};
use tempfile::TempDir;

fn run_ok(program: &Path, cwd: Option<&Path>, args: &[&str]) {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            grit_utils::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{} {} failed: {}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse(git: &Path, repo: &Path, spec: &str) -> String {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", spec]).current_dir(repo);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            grit_utils::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().expect("rev-parse");
    assert!(out.status.success(), "rev-parse {spec}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn synth_s_bare(git: &Path, work: &Path) -> PathBuf {
    let wt = work.join("synth-s");
    generate_synth_repo(&wt, SynthTier::S).expect("generate synth S");
    let bare = work.join("synth-s.git");
    run_ok(
        git,
        Some(work),
        &[
            "clone",
            "-q",
            "--bare",
            wt.to_str().unwrap(),
            bare.file_name().unwrap().to_str().unwrap(),
        ],
    );
    bare
}

#[test]
fn repeated_push_prepare_requires_object_transfer() {
    let git = match resolve_binary("git", None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: {e}");
            return;
        }
    };

    let tmp = TempDir::new().expect("tempdir");
    let bare = synth_s_bare(&git, tmp.path());
    let meta = ensure_campaign_bare_network_meta(&git, &bare, true).expect("meta");
    ensure_network_client_repo(&git, &bare).expect("client");
    let client = meta.client_repo.clone();
    let url = file_url(&bare).expect("file url");
    configure_remote(&git, &client, &url, &meta.default_branch).expect("remote");

    prepare_network_push(&git, &bare).expect("prepare 1");
    let tip1 = rev_parse(&git, &client, "HEAD");
    assert_push_target_lacks_client_tip(&git, &bare, &client).expect("precondition 1");
    run_ok(
        &git,
        Some(&client),
        &["push", "-q", "origin", meta.default_branch.as_str()],
    );
    assert!(
        git_object_exists(&git, &bare, &tip1),
        "bare should store tip1 after push"
    );

    prepare_network_push(&git, &bare).expect("prepare 2");
    let tip2 = rev_parse(&git, &client, "HEAD");
    assert_ne!(tip1, tip2, "each prepare must build a distinct client tip");
    assert_push_target_lacks_client_tip(&git, &bare, &client).expect("precondition 2");
    assert!(
        !git_object_exists(&git, &bare, &tip2),
        "bare must not already have the new tip before the second push"
    );
}
