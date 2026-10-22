//! `grit tag` positional target, `-F`, `--object` alias, and `--dry-run`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn repo() -> PathBuf {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let out = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&root)
        .output()
        .expect("git init");
    assert!(out.status.success());
    fs::write(root.join("f"), b"x\n").unwrap();
    Command::new("git")
        .args(["add", "f"])
        .current_dir(&root)
        .status()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-qm", "one"])
        .current_dir(&root)
        .status()
        .expect("git commit");
    // Leak tempdir by returning path — tests are short-lived.
    std::mem::forget(tmp);
    root
}

fn gs(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GS)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit")
}

#[test]
fn positional_target_matches_hidden_object_alias() {
    let root = repo();
    let parent = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD~0"])
            .current_dir(&root)
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .trim()
    .to_owned();

    let pos = gs(&root, &["tag", "t-pos", &parent, "--json"]);
    assert!(pos.status.success(), "{:?}", pos.stderr);
    let pos_json: serde_json::Value = serde_json::from_slice(&pos.stdout).unwrap();

    let alias = gs(&root, &["tag", "t-alias", "--object", &parent, "--json"]);
    assert!(alias.status.success(), "{:?}", alias.stderr);
    let alias_json: serde_json::Value = serde_json::from_slice(&alias.stdout).unwrap();

    assert_eq!(pos_json["peeled"], alias_json["peeled"]);
    assert_eq!(pos_json["kind"], alias_json["kind"]);
}

#[test]
fn message_file_and_stdin() {
    let root = repo();
    let msg_path = root.join("msg.txt");
    fs::write(&msg_path, "from file\n# ignored\n").unwrap();

    let file = gs(
        &root,
        &[
            "tag",
            "from-file",
            "-F",
            msg_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(file.status.success(), "{:?}", file.stderr);
    let file_json: serde_json::Value = serde_json::from_slice(&file.stdout).unwrap();
    assert_eq!(file_json["kind"], "annotated");

    let stdin = Command::new(GS)
        .args(["tag", "from-stdin", "-F", "-", "--json"])
        .current_dir(&root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    use std::io::Write;
    let mut child = stdin;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from stdin\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    let stdin_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(stdin_json["kind"], "annotated");
}

#[test]
fn dry_run_create_and_delete_leave_refs() {
    let root = repo();
    let dry = gs(&root, &["tag", "ghost", "-m", "x", "--dry-run", "--json"]);
    assert!(dry.status.success());
    let dry_json: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_json["dry_run"], true);

    let list = gs(&root, &["tag", "--json"]);
    assert!(list.status.success());
    let list_json: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(list_json["tags"].as_array().unwrap().is_empty());

    let stay = gs(&root, &["tag", "stay"]);
    assert!(stay.status.success(), "{:?}", stay.stderr);
    let del = gs(&root, &["tag", "-d", "stay", "--dry-run", "--json"]);
    assert!(del.status.success());
    let del_json: serde_json::Value = serde_json::from_slice(&del.stdout).unwrap();
    assert_eq!(del_json["dry_run"], true);

    let list2 = gs(&root, &["tag", "--json"]);
    let names: Vec<_> = list2_json_tags(&list2);
    assert!(names.iter().any(|n| n == "stay"));
}

fn list2_json_tags(out: &std::process::Output) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}
