//! A `git` client cloning from `grit upload-pack` sees grit's `promisor-remote`
//! advertisement and answers it. Needs Git 2.49 or newer; older `git` prints SKIP.

use std::path::Path;
use std::process::Command;

use grit_test_support::{git_at_least, git_cmd};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> String {
    git_cmd(args)
        .in_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .suc()
        .stdout
}

#[test]
fn git_clone_accepts_the_lop_grit_advertises() {
    if !git_at_least(2, 49) {
        eprintln!("SKIP: git older than 2.49 has no promisor-remote capability");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let work = base.join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("small"), "small\n").unwrap();
    std::fs::write(work.join("big"), vec![b'x'; 20_000]).unwrap();
    git(&work, &["add", "."]);
    git(
        &work,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "one",
        ],
    );
    git(
        base,
        &["clone", "-q", "--bare", "--no-local", "work", "server"],
    );
    git(
        base,
        &["clone", "-q", "--bare", "--no-local", "work", "lop"],
    );
    let server = base.join("server");
    let lop = base.join("lop");
    let lop_url = format!("file://{}", lop.display());
    for (k, v) in [
        ("promisor.advertise", "true"),
        ("remote.lop.url", lop_url.as_str()),
        ("remote.lop.promisor", "true"),
        ("uploadpack.allowFilter", "true"),
    ] {
        git(&server, &["config", k, v]);
    }
    for (k, v) in [
        ("uploadpack.allowFilter", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
    ] {
        git(&lop, &["config", k, v]);
    }

    let trace = base.join("trace.txt");
    let out = Command::new("git")
        .current_dir(base)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TRACE_PACKET", &trace)
        .args([
            "-c",
            "protocol.version=2",
            "clone",
            "-q",
            "--upload-pack",
            &format!("'{GRIT}' upload-pack"),
            "--filter=blob:limit=5k",
            "-c",
            &format!("remote.lop.url={lop_url}"),
            "-c",
            "remote.lop.promisor=true",
            "-c",
            "remote.lop.fetch=+refs/heads/*:refs/remotes/lop/*",
            "-c",
            "promisor.acceptfromserver=All",
        ])
        .arg(format!("file://{}", server.display()))
        .arg("client")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let trace = std::fs::read_to_string(trace).unwrap();
    assert!(
        trace.contains(&format!("clone< promisor-remote=name=lop,url={lop_url}")),
        "grit advertised the LOP:\n{trace}"
    );
    assert!(
        trace.contains("clone> promisor-remote=lop"),
        "git accepted it:\n{trace}"
    );
    assert_eq!(
        std::fs::read(base.join("client/big")).unwrap().len(),
        20_000
    );
}
