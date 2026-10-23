//! `-n` / `--limit` behave consistently across list commands.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn gs_ok(repo: &Path, args: &[&str]) {
    let out = Command::new(GS)
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "grit {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn gs_json(repo: &Path, args: &[&str]) -> Value {
    gs_ok(repo, args);
    let out = Command::new(GS)
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run");
    serde_json::from_slice(&out.stdout).expect("json")
}

struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new(_name: &str) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            dir: tempfile::tempdir()?,
        })
    }
    fn child(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }
}

fn assert_two_rows_truncated(v: &Value) {
    let rows = count_rows(v);
    assert_eq!(rows, 2, "expected 2 rows: {v}");
    assert_eq!(v["truncated"], Value::Bool(true), "{v}");
}

fn count_rows(v: &Value) -> usize {
    if let Some(c) = v.get("commits").and_then(|c| c.as_array()) {
        return c.len();
    }
    if let Some(b) = v.get("branches").and_then(|c| c.as_array()) {
        return b.len();
    }
    if let Some(t) = v.get("tags").and_then(|c| c.as_array()) {
        return t.len();
    }
    if v.get("action").and_then(|a| a.as_str()) == Some("list") {
        if let Some(n) = v.get("notes").and_then(|c| c.as_array()) {
            return n.len();
        }
        if let Some(h) = v.get("hooks").and_then(|c| c.as_array()) {
            return h.len();
        }
    }
    if let Some(r) = v.get("references").and_then(|c| c.as_array()) {
        return r.len();
    }
    0
}

#[test]
fn limit_flag_consistency() -> TestResult {
    let scratch = Scratch::new("limit")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    for i in 0..5 {
        fs::write(repo.join("f.txt"), format!("{i}\n"))?;
        gs_ok(&repo, &["add", "f.txt"]);
        gs_ok(&repo, &["commit", &format!("c{i}")]);
        gs_ok(&repo, &["notes", "add", "-m", &format!("note {i}"), "HEAD"]);
    }
    for name in ["b1", "b2", "b3"] {
        gs_ok(&repo, &["branch", name]);
    }
    for name in ["t1", "t2", "t3"] {
        gs_ok(&repo, &["tag", name, "-m", name]);
    }
    let hooks_dir = repo.join(".git").join("hooks");
    for name in ["z-hook-a", "z-hook-b", "z-hook-c"] {
        fs::write(hooks_dir.join(name), "#!/bin/sh\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(hooks_dir.join(name), fs::Permissions::from_mode(0o755))?;
        }
    }

    for (cmd, max_count_alias) in [
        (vec!["log"], true),
        (vec!["revs", "HEAD"], true),
        (vec!["branch"], true),
        (vec!["tag", "-l"], false),
        (vec!["notes"], false),
        (vec!["hooks"], false),
    ] {
        for flag in ["-n", "--limit"] {
            let mut args = cmd.clone();
            args.push(flag);
            args.push("2");
            args.push("--json");
            let v = gs_json(&repo, &args);
            assert_two_rows_truncated(&v);
        }
        if max_count_alias {
            let mut args = cmd.clone();
            args.push("--max-count");
            args.push("2");
            args.push("--json");
            let v = gs_json(&repo, &args);
            assert_two_rows_truncated(&v);
        }
    }

    let bundle = repo.join("x.bundle");
    gs_ok(
        &repo,
        &["bundle", "create", bundle.to_str().unwrap(), "--all"],
    );
    for flag in ["-n", "--limit"] {
        let v = gs_json(
            &repo,
            &[
                "bundle",
                "list",
                bundle.to_str().unwrap(),
                flag,
                "1",
                "--json",
            ],
        );
        assert_eq!(v["references"].as_array().unwrap().len(), 1);
        assert_eq!(v["truncated"], Value::Bool(true));
    }

    Ok(())
}
