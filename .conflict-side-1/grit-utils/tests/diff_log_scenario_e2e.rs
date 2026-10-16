//! End-to-end tests for diff and log benchmark scenarios.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_utils::binary::{require_hyperfine, resolve_binary};
use grit_utils::fixture::{create_repo_with_history, remove_dir_robust, scratch_dir};
use grit_utils::scenarios::{
    log_bench_expected_grit_invocations, log_bench_grit_shell, validate_log_bench_grit_shell,
    LOG_BENCH_COMMIT_COUNT, LOG_BENCH_GRIT_PAGE_COUNT,
};

fn build_grit_executable() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "grit-cli", "--bin", "grit", "-q"])
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build grit-cli");
    assert!(status.success(), "cargo build grit-cli failed");
    let grit = workspace.join("target/debug/grit");
    grit.canonicalize().unwrap_or(grit)
}

fn run_bench(subcmd: &str, id: &str) {
    if require_hyperfine().is_err() {
        eprintln!("skipping {subcmd} e2e: hyperfine not on PATH");
        return;
    }
    let grit = build_grit_executable();
    let bench = std::path::Path::new(env!("CARGO_BIN_EXE_grit-bench"));
    let output = Command::new(bench)
        .args([
            subcmd,
            "--sizes",
            "100",
            "--warmup",
            "1",
            "--min-runs",
            "2",
            "--format",
            "json",
            "--grit",
            &grit.to_string_lossy(),
        ])
        .output()
        .unwrap_or_else(|e| panic!("run grit-bench {subcmd}: {e}"));
    assert!(
        output.status.success(),
        "grit-bench {subcmd} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["scenarios"][0]["id"], id);
    assert!(report["scenarios"][0]["git"]["median_ms"].as_f64().unwrap() > 0.0);
    assert!(
        report["scenarios"][0]["grit"]["median_ms"]
            .as_f64()
            .unwrap()
            > 0.0
    );
}

fn count_commit_lines(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('→') && !t.starts_with("→")
        })
        .count()
}

fn write_grit_wrapper(real_grit: &Path, counter_file: &Path, fail_on_call: Option<u32>) -> PathBuf {
    let wrapper = counter_file.with_file_name("grit-wrap.sh");
    let fail_clause = fail_on_call.map_or(String::new(), |n| {
        format!(
            r#"n=$(($(cat '{c}' 2>/dev/null || echo 0)+1))
echo "$n" > '{c}'
if [ "$n" -eq {n} ]; then exit 42; fi
"#,
            c = counter_file.display(),
            n = n
        )
    });
    let count_clause = if fail_on_call.is_some() {
        String::new()
    } else {
        format!(
            r#"echo $(($(cat '{c}' 2>/dev/null || echo 0)+1)) > '{c}'"#,
            c = counter_file.display()
        )
    };
    let body = if fail_on_call.is_some() {
        fail_clause
    } else {
        format!("{count_clause}\n")
    };
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n{body}exec {real} \"$@\"\n",
            body = body,
            real = shell_escape(real_grit)
        ),
    )
    .expect("write wrapper");
    let mut perms = fs::metadata(&wrapper).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&wrapper, perms).expect("chmod wrapper");
    let _ = fs::remove_file(counter_file);
    wrapper
}

fn shell_escape(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

fn run_log_workload_in_repo(grit: &Path, repo: &Path) -> std::process::Output {
    let script = log_bench_grit_shell(grit);
    Command::new("sh")
        .arg("-c")
        .arg(&script)
        .current_dir(repo)
        .env("NO_COLOR", "1")
        .output()
        .expect("run log workload shell")
}

fn log_bench_workload_cardinality_and_invocations() {
    let git = resolve_binary("git", None).expect("git");
    let grit = build_grit_executable();
    remove_dir_robust(&scratch_dir());
    let repo = create_repo_with_history(&git, 100, LOG_BENCH_COMMIT_COUNT).expect("fixture");

    let git_out = Command::new(&git)
        .args(["log", "-100", "--oneline"])
        .current_dir(&repo)
        .output()
        .expect("git log");
    assert!(git_out.status.success());
    assert_eq!(
        count_commit_lines(&String::from_utf8_lossy(&git_out.stdout)),
        LOG_BENCH_COMMIT_COUNT
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = tmp.path().join("invocations");
    let wrapper = write_grit_wrapper(&grit, &counter, None);
    let script = log_bench_grit_shell(&wrapper);
    validate_log_bench_grit_shell(&script, &wrapper).expect("shell shape");
    assert_eq!(
        log_bench_expected_grit_invocations(),
        LOG_BENCH_GRIT_PAGE_COUNT
    );

    let out = run_log_workload_in_repo(&wrapper, &repo);
    assert!(
        out.status.success(),
        "log workload failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        count_commit_lines(&String::from_utf8_lossy(&out.stdout)),
        LOG_BENCH_COMMIT_COUNT
    );
    let n: u32 = fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("parse count");
    assert_eq!(n, LOG_BENCH_GRIT_PAGE_COUNT as u32);

    let fail_counter = tmp.path().join("fail-count");
    let fail_wrapper = write_grit_wrapper(&grit, &fail_counter, Some(3));
    let fail_out = run_log_workload_in_repo(&fail_wrapper, &repo);
    assert!(
        !fail_out.status.success(),
        "expected failing grit page to fail the workload"
    );

    remove_dir_robust(&scratch_dir());
}

#[test]
fn diff_and_log_scenarios_end_to_end() {
    // `/tmp/grit-bench-scratch` is shared; keep workloads sequential.
    log_bench_workload_cardinality_and_invocations();
    run_bench("diff", "diff-100");
    run_bench("log", "log-100");
}
