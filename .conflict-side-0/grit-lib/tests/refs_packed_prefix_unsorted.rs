//! Prefix listing on `packed-refs` without the `sorted` header trait (Git-compatible).

use std::fs;
use std::process::Command;

use grit_lib::refs::list_refs_glob;
use grit_lib::refs::store::paths::packed_refs_path;
use tempfile::tempdir;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn unsorted_packed_refs_prefix_glob_matches_git_for_each_ref() {
    if !git_available() {
        return;
    }
    let dir = tempdir().expect("tempdir");
    let git_dir = dir.path();
    let init = Command::new("git")
        .args(["init", "--bare"])
        .current_dir(git_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    assert!(init.success(), "git init --bare failed");
    let hash = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(git_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(b"blob\n\n")?;
            }
            child.wait_with_output()
        })
        .expect("git hash-object");
    assert!(hash.status.success(), "git hash-object failed");
    let oid = String::from_utf8(hash.stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    assert_eq!(oid.len(), 40, "expected full hex oid");
    fs::write(
        packed_refs_path(git_dir),
        format!(
            "# pack-refs with: peeled fully-peeled\n{oid} refs/heads/zzz\n{oid} refs/heads/aaa\n{oid} refs/heads/main\n"
        ),
    )
    .expect("packed-refs");

    let grit: Vec<String> = list_refs_glob(git_dir, "refs/heads/a*")
        .expect("list_refs_glob")
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    let git_out = Command::new("git")
        .current_dir(git_dir)
        .args(["for-each-ref", "refs/heads/a*"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git for-each-ref");
    assert!(
        git_out.status.success(),
        "git for-each-ref failed: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let git_names: Vec<String> = String::from_utf8_lossy(&git_out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect();

    assert_eq!(grit, git_names);
    assert_eq!(grit, ["refs/heads/aaa"]);
}
