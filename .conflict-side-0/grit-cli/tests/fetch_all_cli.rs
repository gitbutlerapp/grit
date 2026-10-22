//! CLI JSON for `grit fetch --all`.

use std::error::Error;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?;
    assert!(status.success(), "git {args:?}");
    Ok(())
}

fn grit_json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full: Vec<&str> = vec!["--json"];
    full.extend_from_slice(args);
    let out = Command::new(GS)
        .args(full)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "json parse (status={}): stdout={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    });
    value
}

#[test]
fn fetch_all_json_shape_and_partial_failure_exit() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path();
    let bare = base.join("origin.git");
    git(
        base,
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    )?;
    let consumer = base.join("consumer");
    git(
        base,
        &["init", "-q", "-b", "main", consumer.to_str().unwrap()],
    )?;
    git(
        &consumer,
        &["remote", "add", "good", bare.to_str().unwrap()],
    )?;
    git(
        &consumer,
        &["remote", "add", "bad", "file:///no/such/repo.git"],
    )?;

    let out = Command::new(GS)
        .args(["fetch", "--all", "--json"])
        .current_dir(&consumer)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(!out.status.success(), "expected exit 1 when a remote fails");

    let json = serde_json::from_slice::<serde_json::Value>(&out.stdout)?;
    assert!(json.get("remote").is_none());
    assert!(json.get("remotes").is_some());
    let remotes = json["remotes"].as_array().expect("remotes array");
    assert_eq!(remotes.len(), 2);
    assert!(remotes
        .iter()
        .any(|r| r["remote"] == "good" && r.get("error").is_none()));
    assert!(remotes
        .iter()
        .any(|r| r["remote"] == "bad" && r["error"].is_string()));
    assert!(json["updated"].is_number());

    git(&consumer, &["remote", "remove", "bad"])?;
    let ok_only = grit_json(&consumer, &["fetch", "--all"]);
    assert_eq!(ok_only["remotes"].as_array().map(|a| a.len()), Some(1));
    assert!(ok_only["remotes"][0].get("error").is_none());

    Ok(())
}
