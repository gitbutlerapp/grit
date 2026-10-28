//! `grit clone --config` and `--blobs auto` against a server that advertises a
//! large-object promisor, all served by `grit upload-pack` through an ssh shim.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use grit_test_support::grit_cmd;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");
const TOKEN: &str = "s3cret-lop-token";

/// An ssh stand-in that runs the requested service with `grit` on this machine and logs
/// each remote command to `ssh.log`.
fn fake_ssh(dir: &Path) -> PathBuf {
    let script = dir.join("fake-ssh.sh");
    let log = dir.join("ssh.log").display().to_string();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nwhile [ \"$1\" = \"-p\" ] || [ \"$1\" = \"-o\" ]; do shift 2; done\n\
             shift\ncmd=$*\necho \"$cmd\" >> '{log}'\ncase \"$cmd\" in\n  \"git-upload-pack \"*) cmd=\"'{GRIT}' upload-pack ${{cmd#git-upload-pack }}\" ;;\nesac\n\
             eval \"exec $cmd\"\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn grit_ok(dir: &Path, args: &[&str]) -> String {
    grit_cmd(args).in_dir(dir).suc().stdout
}

/// A work repository with a small and a 20 KiB file, a bare `server` clone of it that
/// advertises `lop`, and `lop`, another full copy that serves any object.
fn setup(root: &Path) -> (PathBuf, PathBuf) {
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    grit_ok(&work, &["init", "-b", "main"]);
    std::fs::write(work.join("small.txt"), "small\n").unwrap();
    std::fs::write(work.join("big.bin"), vec![b'x'; 20 * 1024]).unwrap();
    grit_ok(&work, &["add", "."]);
    grit_ok(&work, &["commit", "-m", "seed"]);
    let server = root.join("server.git");
    let lop = root.join("lop.git");
    for dest in [&server, &lop] {
        grit_ok(
            root,
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        );
        for (k, v) in [
            ("uploadpack.allowFilter", "true"),
            ("uploadpack.allowAnySHA1InWant", "true"),
        ] {
            grit_ok(dest, &["config", k, v]);
        }
    }
    let lop_url = format!("ssh://localhost{}", lop.display());
    for (k, v) in [
        ("promisor.advertise", "true"),
        ("promisor.sendFields", "partialCloneFilter,token"),
        ("remote.lop.url", lop_url.as_str()),
        ("remote.lop.promisor", "true"),
        ("remote.lop.partialCloneFilter", "blob:limit=5k"),
        ("remote.lop.token", TOKEN),
    ] {
        grit_ok(&server, &["config", k, v]);
    }
    (server, lop)
}

fn clone_args(server: &Path, lop: &Path, dest: &str, mode: &str) -> Vec<String> {
    let mut args = vec![
        "clone".to_owned(),
        "--blobs".to_owned(),
        "auto".to_owned(),
        "--config".to_owned(),
        format!("remote.lop.url=ssh://localhost{}", lop.display()),
        "--config".to_owned(),
        "remote.lop.promisor".to_owned(),
        "--config".to_owned(),
        "promisor.acceptFromServer=knownUrl".to_owned(),
        "--config".to_owned(),
        "promisor.storeFields=token".to_owned(),
        format!("ssh://localhost{}", server.display()),
        dest.to_owned(),
    ];
    if !mode.is_empty() {
        args.insert(0, mode.to_owned());
    }
    args
}

fn run_clone(root: &Path, ssh: &Path, args: &[String]) -> String {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    grit_cmd(&args)
        .in_dir(root)
        .env("GIT_SSH_COMMAND", ssh)
        .suc()
        .stdout
}

#[test]
fn clone_config_entries_are_kept_in_the_new_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _) = setup(tmp.path());
    grit_ok(
        tmp.path(),
        &[
            "clone",
            "--config",
            "core.autocrlf=false",
            "--config",
            "remote.extra.fetch=+refs/heads/a:refs/x/a",
            "--config",
            "remote.extra.fetch=+refs/heads/b:refs/x/b",
            "--config",
            "grit.flag",
            server.to_str().unwrap(),
            "plain",
        ],
    );
    let dest = tmp.path().join("plain");
    assert_eq!(grit_ok(&dest, &["config", "core.autocrlf"]).trim(), "false");
    assert_eq!(grit_ok(&dest, &["config", "grit.flag"]).trim(), "true");
    let all = grit_ok(&dest, &["config", "--all", "remote.extra.fetch"]);
    assert!(
        all.contains("refs/x/a") && all.contains("refs/x/b"),
        "{all}"
    );
}

#[test]
fn blobs_auto_accepts_the_lop_and_reports_it_without_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, lop) = setup(tmp.path());
    let ssh = fake_ssh(tmp.path());

    let json = run_clone(
        tmp.path(),
        &ssh,
        &clone_args(&server, &lop, "c-json", "--json"),
    );
    assert!(!json.contains(TOKEN), "{json}");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["filter_auto"], true);
    assert_eq!(v["blobs"], 5120);
    let lop_json = &v["promisor_remotes"][0];
    assert_eq!(lop_json["name"], "lop");
    assert_eq!(lop_json["accepted"], true);
    assert_eq!(lop_json["filter"], "blob:limit=5k");
    assert!(lop_json["rejected"].is_null());
    assert_eq!(v["stored_fields"][0]["field"], "token");
    assert_eq!(v["files"], 2);
    // The checkout's missing blob was asked of the LOP, not of the server again.
    let log = std::fs::read_to_string(tmp.path().join("ssh.log")).unwrap();
    let lop_path = lop.display().to_string();
    let server_path = server.display().to_string();
    assert_eq!(
        log.lines().filter(|l| l.contains(&lop_path)).count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.lines().filter(|l| l.contains(&server_path)).count(),
        1,
        "{log}"
    );

    let dest = tmp.path().join("c-json");
    assert_eq!(
        grit_ok(&dest, &["config", "remote.origin.partialclonefilter"]).trim(),
        "auto"
    );
    assert_eq!(
        grit_ok(&dest, &["config", "remote.lop.token"]).trim(),
        TOKEN
    );
    assert_eq!(
        std::fs::read(dest.join("big.bin")).unwrap().len(),
        20 * 1024
    );

    let human = run_clone(tmp.path(), &ssh, &clone_args(&server, &lop, "c-human", ""));
    assert!(human.contains("lop accepted · blob:limit=5k"), "{human}");
    assert!(human.contains("stored token for lop"), "{human}");
    assert!(!human.contains(TOKEN), "{human}");

    let md = run_clone(
        tmp.path(),
        &ssh,
        &clone_args(&server, &lop, "c-md", "--markdown"),
    );
    assert!(
        md.contains("- **promisor remotes**: `lop` (accepted)"),
        "{md}"
    );
    assert!(!md.contains(TOKEN), "{md}");
}

#[test]
fn full_clone_declines_the_lop() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, lop) = setup(tmp.path());
    let ssh = fake_ssh(tmp.path());
    let args: Vec<String> = vec![
        "--json".into(),
        "clone".into(),
        "--config".into(),
        format!("remote.lop.url=ssh://localhost{}", lop.display()),
        "--config".into(),
        "remote.lop.promisor=true".into(),
        "--config".into(),
        "promisor.acceptFromServer=all".into(),
        format!("ssh://localhost{}", server.display()),
        "full".into(),
    ];
    let v: serde_json::Value = serde_json::from_str(&run_clone(tmp.path(), &ssh, &args)).unwrap();
    assert_eq!(v["promisor_remotes"][0]["accepted"], false);
    assert_eq!(v["promisor_remotes"][0]["rejected"], "not_partial");
}

#[test]
fn clone_reports_promisor_config_warnings() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, lop) = setup(tmp.path());
    let ssh = fake_ssh(tmp.path());
    let args = |mode: &str, dest: &str| -> Vec<String> {
        let mut args: Vec<String> = vec![
            "clone".into(),
            "--blobs".into(),
            "none".into(),
            "--config".into(),
            format!("remote.lop.url=ssh://localhost{}", lop.display()),
            "--config".into(),
            "remote.lop.promisor=true".into(),
            "--config".into(),
            "promisor.acceptFromServer=knownUrls".into(),
            format!("ssh://localhost{}", server.display()),
            dest.into(),
        ];
        if !mode.is_empty() {
            args.insert(0, mode.into());
        }
        args
    };
    let v: serde_json::Value =
        serde_json::from_str(&run_clone(tmp.path(), &ssh, &args("--json", "c-json"))).unwrap();
    assert_eq!(v["promisor_remotes"][0]["rejected"], "policy_none");
    let warning = &v["promisor_warnings"][0];
    assert_eq!(warning["code"], "unknown_accept_value");
    assert_eq!(warning["key"], "promisor.acceptFromServer");
    assert_eq!(warning["value"], "knownUrls");

    let human = run_clone(tmp.path(), &ssh, &args("", "c-human"));
    assert!(
        human.contains("warning: promisor.acceptFromServer = knownUrls is not a known policy"),
        "{human}"
    );
    let md = run_clone(tmp.path(), &ssh, &args("--markdown", "c-md"));
    assert!(
        md.contains("- **promisor warning** (`unknown_accept_value`)"),
        "{md}"
    );
}
