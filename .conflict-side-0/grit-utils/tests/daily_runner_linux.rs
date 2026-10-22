//! Integration tests for the daily direct-spawn runner (Linux RSS semantics).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use grit_utils::capped_run::{TerminationKind, EXIT_TIMEOUT};
use grit_utils::daily::runner::{
    run_ab_bench, run_timed_via_helper, AbBenchRequest, BenchArgv, RunLimits, RunPolicy,
};

fn bench_exe() -> &'static str {
    env!("CARGO_BIN_EXE_grit-bench")
}

fn python_alloc_child(_parent_mib: u32, child_mib: u32) -> BenchArgv {
    let script = format!(
        "import subprocess, sys; subprocess.run([sys.executable, '-c', 'bytearray({child_mib} * 1024 * 1024)']); print('ok')"
    );
    BenchArgv::new("python3", ["-c", &script])
}

#[test]
fn peak_rss_counts_grandchild_allocation() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let cmd = python_alloc_child(0, 256);
    let sample = run_timed_via_helper(
        dir.path(),
        &cmd,
        &RunLimits::default(),
        Path::new(bench_exe()),
    )
    .expect("timed run");
    assert_eq!(sample.termination, TerminationKind::Success);
    assert!(
        sample.peak_rss_bytes >= 256 * 1024 * 1024,
        "expected >=256 MiB peak RSS, got {}",
        sample.peak_rss_bytes
    );
}

#[test]
fn peak_rss_is_max_over_runs() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let helper = Path::new(bench_exe());
    let small = BenchArgv::new("python3", ["-c", "bytearray(32 * 1024 * 1024)"]);
    let large = BenchArgv::new("python3", ["-c", "bytearray(96 * 1024 * 1024)"]);
    let s1 =
        run_timed_via_helper(dir.path(), &small, &RunLimits::default(), helper).expect("small");
    let s2 =
        run_timed_via_helper(dir.path(), &large, &RunLimits::default(), helper).expect("large");
    let peak = s1.peak_rss_bytes.max(s2.peak_rss_bytes);
    assert!(s2.peak_rss_bytes > s1.peak_rss_bytes);
    assert!(peak >= 96 * 1024 * 1024);
}

#[test]
fn prepare_runs_before_every_iteration() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let counter = dir.path().join("prepare.count");
    fs::write(&counter, "0").expect("init counter");
    let inc_script = format!(
        "p=r'{}'; n=int(open(p).read()); open(p,'w').write(str(n+1))",
        counter.display()
    );
    let prepare = BenchArgv::new("python3", ["-c", &inc_script]);
    let noop = BenchArgv::new("python3", ["-c", "pass"]);
    let policy = RunPolicy {
        warmups_per_tool: 0,
        min_runs_per_tool: 2,
        max_runs_per_tool: 2,
        min_total_timed_duration: Duration::from_millis(0),
        scenario_time_budget: Duration::from_secs(30),
    };
    run_ab_bench(&AbBenchRequest {
        cwd: dir.path(),
        git: &noop,
        grit: &noop,
        prepare: Some(&prepare),
        policy: &policy,
        limits: &RunLimits::default(),
        measure_helper: Path::new(bench_exe()),
        correctness: None,
        post_timed: None,
        pre_timed: None,
    })
    .expect("ab bench");
    let count: u32 = fs::read_to_string(&counter)
        .expect("read counter")
        .trim()
        .parse()
        .expect("parse counter");
    // One prepare before each timed iteration (git, grit, git, grit).
    assert_eq!(count, 4, "prepare should run before every timed iteration");
}

#[test]
fn interleaves_git_and_grit() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("order.log");
    fs::write(&log, "").expect("init log");
    let git_script = format!(
        "open('{}','a').write('g'); import time; time.sleep(0.01)",
        log.display()
    );
    let grit_script = format!(
        "open('{}','a').write('r'); import time; time.sleep(0.01)",
        log.display()
    );
    let git = BenchArgv::new("python3", ["-c", &git_script]);
    let grit = BenchArgv::new("python3", ["-c", &grit_script]);
    let policy = RunPolicy {
        warmups_per_tool: 0,
        min_runs_per_tool: 2,
        max_runs_per_tool: 2,
        min_total_timed_duration: Duration::from_millis(0),
        scenario_time_budget: Duration::from_secs(30),
    };
    run_ab_bench(&AbBenchRequest {
        cwd: dir.path(),
        git: &git,
        grit: &grit,
        prepare: None,
        policy: &policy,
        limits: &RunLimits::default(),
        measure_helper: Path::new(bench_exe()),
        correctness: None,
        post_timed: None,
        pre_timed: None,
    })
    .expect("ab bench");
    let order = fs::read_to_string(&log).expect("read order");
    assert_eq!(order, "grgr");
}

#[test]
fn timeout_reports_typed_failure() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let sleep = BenchArgv::new("python3", ["-c", "import time; time.sleep(30)"]);
    let sample = run_timed_via_helper(
        dir.path(),
        &sleep,
        &RunLimits {
            timeout: Some(Duration::from_secs(1)),
            max_rss_bytes: None,
        },
        Path::new(bench_exe()),
    )
    .expect("timed run");
    assert_eq!(sample.termination, TerminationKind::Timeout);

    let out = Command::new(bench_exe())
        .args([
            "measure-direct-run",
            "--cwd",
            dir.path().to_str().expect("utf8"),
            "--",
            "python3",
            "-c",
            "import time; time.sleep(30)",
        ])
        .env(grit_utils::resource::MEASURE_TIMEOUT_SECS_ENV, "1")
        .output()
        .expect("direct timeout");
    assert_eq!(out.status.code(), Some(EXIT_TIMEOUT));
}

#[test]
fn ab_bench_fails_when_timed_command_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ok = BenchArgv::new("/bin/true", Vec::<String>::new());
    let fail = BenchArgv::new("/bin/false", Vec::<String>::new());
    let policy = RunPolicy {
        warmups_per_tool: 0,
        min_runs_per_tool: 1,
        max_runs_per_tool: 1,
        min_total_timed_duration: Duration::from_millis(0),
        scenario_time_budget: Duration::from_secs(30),
    };
    let err = run_ab_bench(&AbBenchRequest {
        cwd: dir.path(),
        git: &ok,
        grit: &fail,
        prepare: None,
        policy: &policy,
        limits: &RunLimits::default(),
        measure_helper: Path::new(bench_exe()),
        correctness: None,
        post_timed: None,
        pre_timed: None,
    })
    .expect_err("failed grit timed run must fail the scenario");
    assert!(err.to_string().contains("grit timed run"), "{err}");
}
