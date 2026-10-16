//! Clone and incremental fetch over `file://` and grit smart HTTP on tier **S** synth fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_utils::binary::resolve_binary;
use grit_utils::network_fixture::{
    ensure_campaign_bare_network_meta, file_url, install_http_repo, REPO_NAME,
};
use grit_utils::network_http::GritHttpServer;
use grit_utils::network_suite::{
    ensure_network_client_repo, prepare_network_clone, prepare_network_fetch_incr,
};
use grit_utils::network_verify::{verify_client_fetch_tip, verify_clone_against_bare};
use grit_utils::synth::{generate_synth_repo, SynthTier};
use tempfile::TempDir;

fn build_network_binaries() -> Option<(PathBuf, PathBuf)> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "grit-cli",
            "-p",
            "grit-http-server",
            "--bin",
            "grit",
            "--bin",
            "grit-http-server",
            "-q",
        ])
        .current_dir(&workspace)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    Some((
        workspace.join("target/debug/grit"),
        workspace.join("target/debug/grit-http-server"),
    ))
}

fn run_ok(program: &Path, cwd: Option<&Path>, args: &[&str]) {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd.output().expect("spawn command");
    assert!(
        out.status.success(),
        "{} {} failed: {}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
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
            bare.to_str().unwrap(),
        ],
    );
    bare
}

#[test]
fn network_synth_s_clone_and_fetch_file_and_grit_http() {
    let git = match resolve_binary("git", None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: {e}");
            return;
        }
    };
    let Some((grit, http_server)) = build_network_binaries() else {
        eprintln!("SKIP: could not build grit / grit-http-server");
        return;
    };

    let tmp = TempDir::new().expect("tempdir");
    let bare = synth_s_bare(&git, tmp.path());
    let meta = ensure_campaign_bare_network_meta(&git, &bare, true).expect("network meta");

    // --- file:// clone (git + grit) ---
    prepare_network_clone(&bare).expect("prepare clone");
    run_ok(
        &git,
        meta.clone_dest.parent().as_deref(),
        &[
            "clone",
            "-q",
            &file_url(&bare).expect("file url"),
            meta.clone_dest.file_name().unwrap().to_str().unwrap(),
        ],
    );
    verify_clone_against_bare(&git, &bare, &meta.clone_dest, &meta.default_branch)
        .expect("git file clone verify");
    prepare_network_clone(&bare).expect("prepare clone");
    run_ok(
        &grit,
        meta.clone_dest.parent().as_deref(),
        &[
            "clone",
            &file_url(&bare).expect("file url"),
            meta.clone_dest.file_name().unwrap().to_str().unwrap(),
        ],
    );
    verify_clone_against_bare(&git, &bare, &meta.clone_dest, &meta.default_branch)
        .expect("grit file clone verify");

    // --- grit-http clone ---
    let http_root = tmp.path().join("http-root");
    install_http_repo(&git, &bare, &http_root).expect("install http repo");
    let grit_server = GritHttpServer::start(&grit, Some(&http_server), http_root)
        .expect("start grit-http-server");
    let grit_url = grit_server.url(REPO_NAME);
    prepare_network_clone(&bare).expect("prepare clone");
    run_ok(
        &grit,
        meta.clone_dest.parent().as_deref(),
        &[
            "clone",
            &grit_url,
            meta.clone_dest.file_name().unwrap().to_str().unwrap(),
        ],
    );
    verify_clone_against_bare(&git, &bare, &meta.clone_dest, &meta.default_branch)
        .expect("grit smart HTTP clone verify");

    // --- incremental fetch (git + grit clients) ---
    prepare_network_fetch_incr(&git, &bare).expect("prepare fetch incr");
    let client = ensure_network_client_repo(&git, &bare).expect("client repo");
    run_ok(&git, Some(&client), &["fetch", "-q", "origin"]);
    verify_client_fetch_tip(&git, &bare, &client, &meta.default_branch).expect("git fetch tip");

    prepare_network_fetch_incr(&git, &bare).expect("prepare fetch incr");
    let client = ensure_network_client_repo(&git, &bare).expect("client repo");
    run_ok(&grit, Some(&client), &["fetch", "origin"]);
    verify_client_fetch_tip(&git, &bare, &client, &meta.default_branch).expect("grit fetch tip");

    // --- incremental fetch over grit smart HTTP (git + grit clients) ---
    let http_fetch_root = tmp.path().join("http-root-fetch");
    install_http_repo(&git, &bare, &http_fetch_root).expect("http mirror for fetch");
    let grit_server = GritHttpServer::start(&grit, Some(&http_server), http_fetch_root.clone())
        .expect("start grit-http-server for fetch");
    let grit_url = grit_server.url(REPO_NAME);
    let http_client = tmp.path().join("http-fetch-client");
    run_ok(
        &grit,
        http_client.parent().as_deref(),
        &[
            "clone",
            &grit_url,
            http_client.file_name().unwrap().to_str().unwrap(),
        ],
    );
    run_ok(
        &git,
        Some(&http_client),
        &["reset", "--hard", &meta.fetch_base_oid],
    );
    prepare_network_fetch_incr(&git, &bare).expect("prepare http fetch incr");
    install_http_repo(&git, &bare, &http_fetch_root).expect("refresh http mirror");
    run_ok(&git, Some(&http_client), &["fetch", "-q", "origin"]);
    verify_client_fetch_tip(&git, &bare, &http_client, &meta.default_branch)
        .expect("git fetch over grit-http");
    run_ok(
        &git,
        Some(&http_client),
        &["reset", "--hard", &meta.fetch_base_oid],
    );
    prepare_network_fetch_incr(&git, &bare).expect("prepare grit http fetch incr");
    install_http_repo(&git, &bare, &http_fetch_root).expect("refresh http mirror");
    run_ok(&grit, Some(&http_client), &["fetch", "origin"]);
    verify_client_fetch_tip(&git, &bare, &http_client, &meta.default_branch)
        .expect("grit fetch over grit-http");
}
