use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

// ── CLI ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "grit-bench", about = "Benchmark grit vs git at scale")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,

    /// Path to grit binary (default: target/release/grit or $PATH)
    #[arg(long, global = true)]
    grit: Option<PathBuf>,

    /// Path to git binary (default: git on $PATH)
    #[arg(long, global = true)]
    git: Option<PathBuf>,

    /// Output format
    #[arg(long, global = true, default_value = "text")]
    format: OutputFormat,

    /// Write output to file instead of stdout
    #[arg(short, long, global = true)]
    output: Option<PathBuf>,

    /// Number of iterations per measurement (results are averaged)
    #[arg(long, global = true, default_value = "5")]
    iterations: usize,
}

#[derive(Subcommand)]
enum Cmd {
    /// Benchmark `status` at various repo sizes
    Status {
        /// File counts to test (comma-separated)
        #[arg(long, value_delimiter = ',', default_values_t = vec![100, 1_000, 10_000, 50_000])]
        sizes: Vec<usize>,
    },
    /// Benchmark `add` at various repo sizes
    Add {
        /// File counts to test (comma-separated)
        #[arg(long, value_delimiter = ',', default_values_t = vec![100, 1_000, 10_000, 50_000])]
        sizes: Vec<usize>,
    },
    /// Benchmark `commit` (stage + commit) at S/M/L repo sizes
    Commit {
        /// File counts to test (comma-separated; default S/M/L)
        #[arg(long, value_delimiter = ',', default_values_t = vec![100, 5_000, 15_000])]
        sizes: Vec<usize>,
    },
    /// Run all benchmarks
    All {
        /// File counts to test (comma-separated)
        #[arg(long, value_delimiter = ',', default_values_t = vec![100, 1_000, 10_000, 50_000])]
        sizes: Vec<usize>,
    },
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Html,
    Json,
}

// ── Data model ───────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct Timing {
    mean_ms: f64,
    min_ms: f64,
    max_ms: f64,
    stddev_ms: f64,
    runs: Vec<f64>,
}

#[derive(Serialize, Clone)]
struct ScalePoint {
    file_count: usize,
    git: Timing,
    grit: Timing,
    speedup: f64, // git_mean / grit_mean
}

#[derive(Serialize, Clone)]
struct BenchResult {
    name: String,
    description: String,
    points: Vec<ScalePoint>,
}

#[derive(Serialize)]
struct Report {
    git_version: String,
    grit_version: String,
    timestamp: String,
    benchmarks: Vec<BenchResult>,
}

// ── Timing helpers ───────────────────────────────────────────────────

fn shell_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains(' ') || s.contains('\'') {
        format!("'{s}'")
    } else {
        s.into_owned()
    }
}

fn isolated_git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com");
}

fn apply_bench_env(cmd_path: &Path, cmd: &mut Command) {
    if cmd_path == Path::new("/bin/sh") || cmd_path.file_name().is_some_and(|n| n == "git") {
        isolated_git_env(cmd);
    }
}

fn measure(cmd_path: &Path, args: &[&str], cwd: &Path, iterations: usize) -> Result<Timing> {
    // Warmup run
    let mut cmd = Command::new(cmd_path);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_bench_env(cmd_path, &mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("failed to run {:?}", cmd_path))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "{} {} failed: {}",
            cmd_path.display(),
            args.join(" "),
            stderr.trim()
        );
    }

    let mut runs = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let mut cmd = Command::new(cmd_path);
        cmd.args(args)
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_bench_env(cmd_path, &mut cmd);
        let out = cmd.output()?;
        let elapsed = start.elapsed();
        if !out.status.success() {
            anyhow::bail!("{} failed on iteration", cmd_path.display());
        }
        runs.push(elapsed.as_secs_f64() * 1000.0);
    }

    let mean = runs.iter().sum::<f64>() / runs.len() as f64;
    let min = runs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = runs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let variance = runs.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / runs.len() as f64;
    let stddev = variance.sqrt();

    Ok(Timing {
        mean_ms: mean,
        min_ms: min,
        max_ms: max,
        stddev_ms: stddev,
        runs,
    })
}

fn measure_with_setup(
    cmd_path: &Path,
    args: &[&str],
    cwd: &Path,
    setup: &dyn Fn() -> Result<()>,
    iterations: usize,
) -> Result<Timing> {
    // Warmup
    setup()?;
    let mut warmup = Command::new(cmd_path);
    warmup
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_bench_env(cmd_path, &mut warmup);
    let out = warmup.output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "{} {} failed: {}",
            cmd_path.display(),
            args.join(" "),
            stderr.trim()
        );
    }

    let mut runs = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        setup()?;
        let start = Instant::now();
        let mut cmd = Command::new(cmd_path);
        cmd.args(args)
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_bench_env(cmd_path, &mut cmd);
        let out = cmd.output()?;
        let elapsed = start.elapsed();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "{} failed on iteration: {}",
                cmd_path.display(),
                stderr.trim()
            );
        }
        runs.push(elapsed.as_secs_f64() * 1000.0);
    }

    let mean = runs.iter().sum::<f64>() / runs.len() as f64;
    let min = runs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = runs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let variance = runs.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / runs.len() as f64;
    let stddev = variance.sqrt();

    Ok(Timing {
        mean_ms: mean,
        min_ms: min,
        max_ms: max,
        stddev_ms: stddev,
        runs,
    })
}

// ── Repo scaffolding ─────────────────────────────────────────────────

fn scratch_dir() -> PathBuf {
    PathBuf::from("/tmp/grit-bench-scratch")
}

fn scratch_dir_named(name: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/grit-bench-scratch-{name}"))
}

fn remove_dir_robust(dir: &Path) {
    // Try up to 3 times — macOS can return ENOTEMPTY transiently on large trees
    for _ in 0..3 {
        if !dir.exists() {
            return;
        }
        if std::fs::remove_dir_all(dir).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Last resort: rm -rf
    let _ = Command::new("rm")
        .args(["-rf", &dir.to_string_lossy()])
        .status();
}

fn create_repo(git: &Path, file_count: usize) -> Result<PathBuf> {
    create_repo_at(git, file_count, scratch_dir())
}

fn create_repo_at(git: &Path, file_count: usize, dir: PathBuf) -> Result<PathBuf> {
    remove_dir_robust(&dir);
    std::fs::create_dir_all(&dir)?;

    // git init
    let mut init = Command::new(git);
    init.args(["init", "-q"]).current_dir(&dir);
    apply_bench_env(git, &mut init);
    let out = init.output()?;
    if !out.status.success() {
        anyhow::bail!("git init failed");
    }

    // Spread files across subdirectories (100 files per dir)
    let files_per_dir = 100;
    let num_dirs = file_count.div_ceil(files_per_dir);
    let mut created = 0;

    for d in 0..num_dirs {
        let subdir = dir.join(format!("d{d:04}"));
        std::fs::create_dir_all(&subdir)?;
        for f in 0..files_per_dir {
            if created >= file_count {
                break;
            }
            let path = subdir.join(format!("f{f:04}.txt"));
            std::fs::write(&path, format!("content {d}/{f}\nline 2\nline 3\n"))?;
            created += 1;
        }
    }

    // git add + commit
    let mut add = Command::new(git);
    add.args(["add", "-A"]).current_dir(&dir);
    apply_bench_env(git, &mut add);
    let out = add.output()?;
    if !out.status.success() {
        anyhow::bail!("git add failed");
    }
    let mut commit = Command::new(git);
    commit
        .args(["commit", "-q", "-m", "initial"])
        .current_dir(&dir);
    apply_bench_env(git, &mut commit);
    let out = commit.output()?;
    if !out.status.success() {
        anyhow::bail!("git commit failed");
    }

    Ok(dir)
}

fn dirty_repo(dir: &Path, count: usize) -> Result<()> {
    // Modify ~10% of files, add some untracked
    let modify_count = count / 10;
    let untracked_count = count / 20;

    let mut modified = 0;
    for entry in walkdir(dir)? {
        if modified >= modify_count {
            break;
        }
        if entry.extension().is_some_and(|e| e == "txt") {
            let mut content = std::fs::read_to_string(&entry)?;
            content.push_str("modified\n");
            std::fs::write(&entry, content)?;
            modified += 1;
        }
    }

    // Add untracked files
    for i in 0..untracked_count {
        let path = dir.join(format!("untracked_{i}.txt"));
        std::fs::write(&path, format!("untracked content {i}\n"))?;
    }

    Ok(())
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walkdir_inner(dir, &mut files)?;
    Ok(files)
}

fn walkdir_inner(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            walkdir_inner(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

// ── Benchmarks ───────────────────────────────────────────────────────

fn bench_status(
    git: &Path,
    grit: &Path,
    sizes: &[usize],
    iterations: usize,
) -> Result<BenchResult> {
    let mut points = Vec::new();

    for &size in sizes {
        eprint!("  status @ {size} files ... ");
        let dir = create_repo(git, size)?;
        dirty_repo(&dir, size)?;

        let git_t = measure(git, &["status", "--porcelain"], &dir, iterations)?;
        let grit_t = measure(grit, &["status", "--porcelain"], &dir, iterations)?;
        let speedup = git_t.mean_ms / grit_t.mean_ms;

        eprintln!(
            "git {:.1}ms  grit {:.1}ms  ({:.2}x)",
            git_t.mean_ms, grit_t.mean_ms, speedup
        );

        points.push(ScalePoint {
            file_count: size,
            git: git_t,
            grit: grit_t,
            speedup,
        });
    }

    Ok(BenchResult {
        name: "status".into(),
        description:
            "git/grit status --porcelain on a dirty worktree (~10% modified, ~5% untracked)".into(),
        points,
    })
}

fn bench_status_clean(
    git: &Path,
    grit: &Path,
    sizes: &[usize],
    iterations: usize,
) -> Result<BenchResult> {
    let mut points = Vec::new();

    for &size in sizes {
        eprint!("  status (clean) @ {size} files ... ");
        let dir = create_repo(git, size)?;

        let git_t = measure(git, &["status", "--porcelain"], &dir, iterations)?;
        let grit_t = measure(grit, &["status", "--porcelain"], &dir, iterations)?;
        let speedup = git_t.mean_ms / grit_t.mean_ms;

        eprintln!(
            "git {:.1}ms  grit {:.1}ms  ({:.2}x)",
            git_t.mean_ms, grit_t.mean_ms, speedup
        );

        points.push(ScalePoint {
            file_count: size,
            git: git_t,
            grit: grit_t,
            speedup,
        });
    }

    Ok(BenchResult {
        name: "status-clean".into(),
        description: "git/grit status --porcelain on a clean worktree (no changes)".into(),
        points,
    })
}

fn bench_add(git: &Path, grit: &Path, sizes: &[usize], iterations: usize) -> Result<BenchResult> {
    let mut points = Vec::new();

    for &size in sizes {
        eprint!("  add @ {size} files ... ");
        let dir = create_repo(git, size)?;

        // For `add`, we need to reset the index before each run
        let git_clone = git.to_path_buf();
        let dir_clone = dir.clone();
        let setup_git = move || {
            // Modify files
            let files = walkdir(&dir_clone)?;
            let modify_count = (files.len() / 5).max(1);
            for f in files.iter().take(modify_count) {
                if f.extension().is_some_and(|e| e == "txt") {
                    std::fs::write(f, "modified for add bench\n")?;
                }
            }
            // Reset index to HEAD
            Command::new(&git_clone)
                .args(["reset", "-q", "HEAD"])
                .current_dir(&dir_clone)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output()?;
            Ok(())
        };

        let grit_clone = grit.to_path_buf();
        let dir_clone2 = dir.clone();
        let setup_grit = move || {
            let files = walkdir(&dir_clone2)?;
            let modify_count = (files.len() / 5).max(1);
            for f in files.iter().take(modify_count) {
                if f.extension().is_some_and(|e| e == "txt") {
                    std::fs::write(f, "modified for add bench\n")?;
                }
            }
            Command::new(&grit_clone)
                .args(["reset", "-q", "HEAD"])
                .current_dir(&dir_clone2)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output()?;
            Ok(())
        };

        let git_t = measure_with_setup(git, &["add", "-A"], &dir, &setup_git, iterations)?;
        let grit_t = measure_with_setup(grit, &["add", "-A"], &dir, &setup_grit, iterations)?;
        let speedup = git_t.mean_ms / grit_t.mean_ms;

        eprintln!(
            "git {:.1}ms  grit {:.1}ms  ({:.2}x)",
            git_t.mean_ms, grit_t.mean_ms, speedup
        );

        points.push(ScalePoint {
            file_count: size,
            git: git_t,
            grit: grit_t,
            speedup,
        });
    }

    Ok(BenchResult {
        name: "add".into(),
        description: "git/grit add -A after modifying ~20% of files".into(),
        points,
    })
}

fn prepare_commit_iteration(dir: &Path, git: &Path) -> Result<()> {
    let files = walkdir(dir)?;
    let modify_count = (files.len() / 5).max(1);
    for f in files.iter().take(modify_count) {
        if f.extension().is_some_and(|e| e == "txt") {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new().append(true).open(f)?;
            writeln!(file, "commit bench change")?;
        }
    }
    let mut reset_git = Command::new(git);
    reset_git
        .args(["reset", "-q", "HEAD"])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    isolated_git_env(&mut reset_git);
    reset_git.output()?;
    Ok(())
}

fn bench_commit(
    git: &Path,
    grit: &Path,
    sizes: &[usize],
    iterations: usize,
) -> Result<BenchResult> {
    let mut points = Vec::new();

    for &size in sizes {
        eprint!("  commit @ {size} files ... ");
        let dir_git = create_repo_at(git, size, scratch_dir_named(&format!("git-{size}")))?;
        let dir_grit = create_repo_at(git, size, scratch_dir_named(&format!("grit-{size}")))?;
        let git_clone = git.to_path_buf();
        let dir_git_setup = dir_git.clone();
        let git_for_git = git_clone.clone();
        let setup_git = move || prepare_commit_iteration(&dir_git_setup, &git_for_git);
        let dir_grit_setup = dir_grit.clone();
        let git_for_grit = git_clone;
        let setup_grit = move || prepare_commit_iteration(&dir_grit_setup, &git_for_grit);

        let git_cmd = format!(
            "{} add -A && {} commit -q -m bench",
            shell_escape(git),
            shell_escape(git)
        );
        let git_t = measure_with_setup(
            Path::new("/bin/sh"),
            &["-c", &git_cmd],
            &dir_git,
            &setup_git,
            iterations,
        )?;
        let grit_t = measure_with_setup(
            grit,
            &["commit", "bench"],
            &dir_grit,
            &setup_grit,
            iterations,
        )?;
        let ratio = grit_t.mean_ms / git_t.mean_ms;
        let speedup = git_t.mean_ms / grit_t.mean_ms;
        eprintln!(
            "git {:.1}ms  grit {:.1}ms  ({:.2}x grit/git)",
            git_t.mean_ms, grit_t.mean_ms, ratio
        );

        points.push(ScalePoint {
            file_count: size,
            git: git_t,
            grit: grit_t,
            speedup,
        });
    }

    Ok(BenchResult {
        name: "commit".into(),
        description:
            "grit commit (stage all + commit) vs git add -A && git commit -q -m on a dirty tree"
                .into(),
        points,
    })
}

// ── Output rendering ─────────────────────────────────────────────────

impl fmt::Display for Timing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}ms (±{:.1})", self.mean_ms, self.stddev_ms)
    }
}

fn render_text(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "grit-bench — {} vs {}\n",
        report.grit_version, report.git_version
    ));
    out.push_str(&format!("{}\n\n", report.timestamp));

    for bench in &report.benchmarks {
        out.push_str(&format!("── {} ──\n", bench.name));
        out.push_str(&format!("{}\n\n", bench.description));
        out.push_str(&format!(
            "{:>10}  {:>20}  {:>20}  {:>8}\n",
            "Files", "git", "grit", "Speedup"
        ));
        out.push_str(&format!("{}\n", "─".repeat(64)));

        for p in &bench.points {
            let marker = if p.speedup >= 1.0 { "▲" } else { "▼" };
            out.push_str(&format!(
                "{:>10}  {:>20}  {:>20}  {:>6.2}x {}\n",
                format_count(p.file_count),
                format!("{}", p.git),
                format!("{}", p.grit),
                p.speedup,
                marker,
            ));
        }
        out.push('\n');
    }

    out
}

fn format_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        format!("{n}")
    }
}

fn render_html(report: &Report) -> String {
    let mut h = String::new();
    h.push_str(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>grit-bench results</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
         max-width: 960px; margin: 2rem auto; padding: 0 1rem; color: #1a1a2e; background: #fafafa; }
  h1 { font-size: 1.6rem; margin-bottom: 0.25rem; }
  .meta { color: #666; font-size: 0.85rem; margin-bottom: 2rem; }
  .bench { margin-bottom: 3rem; }
  .bench h2 { font-size: 1.2rem; margin-bottom: 0.25rem; }
  .bench .desc { color: #555; font-size: 0.85rem; margin-bottom: 1rem; }
  table { width: 100%; border-collapse: collapse; font-size: 0.9rem; }
  th { text-align: right; padding: 0.5rem 0.75rem; border-bottom: 2px solid #ddd;
       font-weight: 600; color: #444; }
  th:first-child { text-align: left; }
  td { text-align: right; padding: 0.5rem 0.75rem; border-bottom: 1px solid #eee; }
  td:first-child { text-align: left; font-weight: 500; }
  .faster { color: #16a34a; font-weight: 600; }
  .slower { color: #dc2626; font-weight: 600; }
  .bar-cell { text-align: left; padding-left: 0; }
  .bar-wrap { display: flex; align-items: center; gap: 0.5rem; height: 1.4rem; }
  .bar { height: 100%; border-radius: 3px; min-width: 2px; }
  .bar.git { background: #94a3b8; }
  .bar.grit { background: #3b82f6; }
  .bar-label { font-size: 0.75rem; color: #888; white-space: nowrap; }
  .legend { display: flex; gap: 1.5rem; margin-bottom: 0.5rem; font-size: 0.8rem; color: #666; }
  .legend span::before { content: ""; display: inline-block; width: 12px; height: 12px;
                          border-radius: 2px; margin-right: 4px; vertical-align: -1px; }
  .legend .lg::before { background: #94a3b8; }
  .legend .lr::before { background: #3b82f6; }
</style>
</head>
<body>
"#,
    );

    h.push_str("<h1>grit-bench</h1>\n");
    h.push_str(&format!(
        "<p class=\"meta\">{} vs {} &mdash; {}</p>\n",
        html_escape(&report.grit_version),
        html_escape(&report.git_version),
        html_escape(&report.timestamp),
    ));

    for bench in &report.benchmarks {
        h.push_str("<div class=\"bench\">\n");
        h.push_str(&format!(
            "<h2>{}</h2>\n<p class=\"desc\">{}</p>\n",
            html_escape(&bench.name),
            html_escape(&bench.description),
        ));

        h.push_str("<div class=\"legend\"><span class=\"lg\">git</span><span class=\"lr\">grit</span></div>\n");
        h.push_str("<table>\n<tr><th>Files</th><th>git</th><th>grit</th><th>Speedup</th><th class=\"bar-cell\">Comparison</th></tr>\n");

        let max_ms = bench
            .points
            .iter()
            .flat_map(|p| [p.git.mean_ms, p.grit.mean_ms])
            .fold(0.0_f64, f64::max);

        for p in &bench.points {
            let class = if p.speedup >= 1.0 { "faster" } else { "slower" };
            let git_pct = if max_ms > 0.0 {
                (p.git.mean_ms / max_ms * 100.0).round() as u32
            } else {
                0
            };
            let grit_pct = if max_ms > 0.0 {
                (p.grit.mean_ms / max_ms * 100.0).round() as u32
            } else {
                0
            };

            h.push_str(&format!(
                "<tr><td>{}</td><td>{:.1}ms <small>±{:.1}</small></td><td>{:.1}ms <small>±{:.1}</small></td>\
                 <td class=\"{class}\">{:.2}x</td>\
                 <td class=\"bar-cell\"><div class=\"bar-wrap\">\
                 <div class=\"bar git\" style=\"width:{git_pct}%\"></div>\
                 <div class=\"bar grit\" style=\"width:{grit_pct}%\"></div>\
                 </div></td></tr>\n",
                format_count(p.file_count),
                p.git.mean_ms, p.git.stddev_ms,
                p.grit.mean_ms, p.grit.stddev_ms,
                p.speedup,
            ));
        }
        h.push_str("</table>\n</div>\n");
    }

    h.push_str("</body>\n</html>\n");
    h
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Main ─────────────────────────────────────────────────────────────

fn resolve_binary(name: &str, user_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = user_path {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        anyhow::bail!("binary not found: {}", p.display());
    }
    if name == "grit" {
        // Try workspace target/release first
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/release/grit");
        if workspace.exists() {
            return Ok(workspace.canonicalize()?);
        }
    }
    which(name)
}

fn which(name: &str) -> Result<PathBuf> {
    let output = Command::new("which").arg(name).output()?;
    if output.status.success() {
        let path = String::from_utf8(output.stdout)?.trim().to_string();
        Ok(PathBuf::from(path))
    } else {
        anyhow::bail!("could not find `{name}` on PATH");
    }
}

fn get_version(bin: &Path) -> String {
    Command::new(bin)
        .arg("version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let git = resolve_binary("git", cli.git.as_deref())?;
    let grit = resolve_binary("grit", cli.grit.as_deref())?;

    let git_version = get_version(&git);
    let grit_version = get_version(&grit);

    eprintln!("git:  {} ({})", git.display(), git_version);
    eprintln!("grit: {} ({})", grit.display(), grit_version);
    eprintln!();

    let iterations = cli.iterations;

    let benchmarks = match &cli.command {
        Cmd::Status { sizes } => {
            eprintln!("Running status benchmarks...");
            let dirty = bench_status(&git, &grit, sizes, iterations)?;
            let clean = bench_status_clean(&git, &grit, sizes, iterations)?;
            vec![dirty, clean]
        }
        Cmd::Add { sizes } => {
            eprintln!("Running add benchmarks...");
            vec![bench_add(&git, &grit, sizes, iterations)?]
        }
        Cmd::Commit { sizes } => {
            eprintln!("Running commit benchmarks...");
            vec![bench_commit(&git, &grit, sizes, iterations)?]
        }
        Cmd::All { sizes } => {
            eprintln!("Running all benchmarks...");
            let dirty = bench_status(&git, &grit, sizes, iterations)?;
            let clean = bench_status_clean(&git, &grit, sizes, iterations)?;
            let add = bench_add(&git, &grit, sizes, iterations)?;
            let commit = bench_commit(&git, &grit, &[100, 5_000, 15_000], iterations)?;
            vec![dirty, clean, add, commit]
        }
    };

    let now = Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S %Z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let report = Report {
        git_version,
        grit_version,
        timestamp: now,
        benchmarks,
    };

    let rendered = match cli.format {
        OutputFormat::Text => render_text(&report),
        OutputFormat::Html => render_html(&report),
        OutputFormat::Json => {
            serde_json::to_string_pretty(&report).context("failed to serialize JSON")?
        }
    };

    if let Some(path) = &cli.output {
        let mut f = std::fs::File::create(path)?;
        f.write_all(rendered.as_bytes())?;
        eprintln!("Results written to {}", path.display());
    } else {
        print!("{rendered}");
    }

    // Cleanup
    remove_dir_robust(&scratch_dir());

    Ok(())
}
