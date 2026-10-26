//! Scenario detail for the daily report: readable command lines and per-phase timings from one
//! traced run per tool (`GIT_TRACE2_PERF` for git, `GRIT_TRACE=<file>` for grit).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::runner::{run_prepare, run_timed_via_helper, BenchArgv, RunLimits};
use super::schema::{DailyPhase, DailyPhases};
use crate::capped_run::TerminationKind;

/// Most phases kept per tool, slowest first.
const MAX_PHASES: usize = 40;

/// A command line as a reader would type it: the `sh -c 'set -e; … > /dev/null 2>&1'`
/// wrapper removed and tool binaries shown by name.
#[must_use]
pub fn display_argv(cmd: &BenchArgv, names: &[(&Path, &str)]) -> String {
    let line = if cmd.program == Path::new("/bin/sh")
        && cmd.args.first().map(String::as_str) == Some("-c")
    {
        let inner = cmd.args.get(1).map(String::as_str).unwrap_or_default();
        let inner = inner.strip_prefix("set -e; ").unwrap_or(inner);
        inner
            .strip_suffix(" > /dev/null 2>&1")
            .unwrap_or(inner)
            .to_string()
    } else {
        std::iter::once(cmd.program.display().to_string())
            .chain(cmd.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let line = names.iter().fold(line, |line, (path, name)| {
        let path = path.display().to_string();
        line.replace(&format!("'{path}'"), name)
            .replace(&path, name)
    });
    // Drop quotes the shell doesn't need: `'push' '-q'` → `push -q`.
    line.split(' ')
        .map(
            |word| match word.strip_prefix('\'').and_then(|w| w.strip_suffix('\'')) {
                Some(inner)
                    if !inner.is_empty()
                        && inner
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "._/:@=+-,^~".contains(c)) =>
                {
                    inner
                }
                _ => word,
            },
        )
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run each tool once more with tracing on (after prepare, like a timed run) and return the
/// phases. Failures are reported in the returned note and never fail the scenario.
pub fn trace_phases(
    cwd: &Path,
    git: &BenchArgv,
    grit: &BenchArgv,
    prepare: Option<&BenchArgv>,
    limits: &RunLimits,
    measure_helper: &Path,
) -> (DailyPhases, Option<String>) {
    let mut phases = DailyPhases::default();
    let mut problems = Vec::new();
    match traced_run(
        cwd,
        git,
        "GIT_TRACE2_PERF",
        "git",
        prepare,
        limits,
        measure_helper,
    ) {
        Ok(text) => phases.git = parse_git_perf(&text),
        Err(err) => problems.push(format!("git trace: {err:#}")),
    }
    match traced_run(
        cwd,
        grit,
        "GRIT_TRACE",
        "grit",
        prepare,
        limits,
        measure_helper,
    ) {
        Ok(text) => phases.grit = parse_grit_trace(&text),
        Err(err) => problems.push(format!("grit trace: {err:#}")),
    }
    let note = (!problems.is_empty()).then(|| problems.join("; "));
    (phases, note)
}

fn traced_run(
    cwd: &Path,
    cmd: &BenchArgv,
    var: &str,
    tool: &str,
    prepare: Option<&BenchArgv>,
    limits: &RunLimits,
    measure_helper: &Path,
) -> Result<String> {
    let path = trace_path(tool);
    let _ = std::fs::remove_file(&path);
    run_prepare(cwd, prepare)?;
    let traced = BenchArgv::new(
        "/usr/bin/env",
        std::iter::once(format!("{var}={}", path.display()))
            .chain(std::iter::once(cmd.program.display().to_string()))
            .chain(cmd.args.iter().cloned()),
    );
    let sample = run_timed_via_helper(cwd, &traced, limits, measure_helper)?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    if sample.termination != TerminationKind::Success {
        anyhow::bail!("traced run failed: {:?}", sample.termination);
    }
    if text.is_empty() {
        anyhow::bail!("traced run wrote no trace to {}", path.display());
    }
    Ok(text)
}

fn trace_path(tool: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "grit-daily-trace-{}-{tool}.txt",
        std::process::id()
    ))
}

#[derive(Default)]
struct Phases(BTreeMap<String, (f64, u32, u32)>);

impl Phases {
    fn add(&mut self, name: String, ms: f64, depth: u32) {
        let entry = self.0.entry(name).or_insert((0.0, 0, depth));
        entry.0 += ms;
        entry.1 += 1;
        entry.2 = entry.2.min(depth);
    }

    fn finish(self) -> Vec<DailyPhase> {
        let mut out: Vec<DailyPhase> = self
            .0
            .into_iter()
            .map(|(name, (ms, count, depth))| DailyPhase {
                name,
                ms: (ms * 1000.0).round() / 1000.0,
                count,
                depth,
            })
            .collect();
        out.sort_by(|a, b| b.ms.partial_cmp(&a.ms).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(MAX_PHASES);
        out
    }
}

/// Regions and child processes from a `GIT_TRACE2_PERF` file, summed by name. Child processes
/// (`git pack-objects`, `git-receive-pack`, …) appear as `run: <command>`.
#[must_use]
pub fn parse_git_perf(text: &str) -> Vec<DailyPhase> {
    let mut phases = Phases::default();
    // Child commands by (process depth, child id), from child_start to child_exit.
    let mut children: BTreeMap<(u32, String), String> = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('|').map(str::trim).collect();
        if fields.len() < 9 {
            continue;
        }
        let depth = fields[1]
            .strip_prefix('d')
            .and_then(|d| d.parse::<u32>().ok())
            .unwrap_or(0);
        let msg = fields[8..].join("|");
        let msg = msg.trim_start_matches('.');
        match fields[3] {
            "region_leave" => {
                let Some(ms) = seconds_ms(fields[6]) else {
                    continue;
                };
                let nest = fields[4]
                    .strip_prefix('r')
                    .and_then(|r| r.parse::<u32>().ok())
                    .unwrap_or(1);
                let label = msg
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("label:"))
                    .unwrap_or(msg);
                phases.add(
                    format!("{}: {label}", fields[7]),
                    ms,
                    depth + nest.saturating_sub(1),
                );
            }
            "child_start" => {
                if let (Some(id), Some(argv)) = (child_id(msg), msg.split_once("argv:[")) {
                    children.insert((depth, id), child_name(argv.1.trim_end_matches(']')));
                }
            }
            "child_exit" => {
                let (Some(id), Some(ms)) = (child_id(msg), seconds_ms(fields[6])) else {
                    continue;
                };
                if let Some(name) = children.remove(&(depth, id)) {
                    phases.add(format!("run: {name}"), ms, depth + 1);
                }
            }
            _ => {}
        }
    }
    phases.finish()
}

/// `git pack-objects --revs …` → `git pack-objects`; `'git-receive-pack '\''/r.git'\''` → `git-receive-pack`.
fn child_name(argv: &str) -> String {
    let mut words = argv
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '\'' || c == '\\'))
        .filter(|w| !w.is_empty() && !w.starts_with('-'));
    let first = words.next().unwrap_or_default();
    let program = first.rsplit('/').next().unwrap_or(first);
    match (program, words.next()) {
        ("git", Some(sub)) => format!("git {sub}"),
        _ => program.to_string(),
    }
}

fn child_id(msg: &str) -> Option<String> {
    let start = msg.find("[ch")?;
    let end = msg[start..].find(']')?;
    Some(msg[start + 1..start + end].to_string())
}

fn seconds_ms(field: &str) -> Option<f64> {
    field.parse::<f64>().ok().map(|s| s * 1000.0)
}

/// Spans from grit's `GRIT_TRACE` tree (`<indent>name: close time.busy=1.23ms …`), summed by name.
#[must_use]
pub fn parse_grit_trace(text: &str) -> Vec<DailyPhase> {
    let mut phases = Phases::default();
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let Some((name, rest)) = line.trim().split_once(": close time.busy=") else {
            continue;
        };
        let busy = rest.split_whitespace().next().unwrap_or_default();
        if let Some(ms) = duration_ms(busy) {
            phases.add(
                name.to_string(),
                ms,
                u32::try_from(indent / 2).unwrap_or(u32::MAX),
            );
        }
    }
    phases.finish()
}

fn duration_ms(value: &str) -> Option<f64> {
    if let Some(v) = value.strip_suffix("ms") {
        v.parse().ok()
    } else if let Some(v) = value.strip_suffix("µs") {
        v.parse::<f64>().ok().map(|us| us / 1000.0)
    } else if let Some(v) = value.strip_suffix("ns") {
        v.parse::<f64>().ok().map(|ns| ns / 1_000_000.0)
    } else if let Some(v) = value.strip_suffix('s') {
        v.parse::<f64>().ok().map(|s| s * 1000.0)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIT_PERF: &str = "\
06:45:41.497327 run-command.c:740            | d0 | main                     | child_start  |     |  0.003051 |           |              | ....[ch0] class:transport/file argv:['git-receive-pack '\\''/tmp/remote.git'\\'']
06:45:41.505817 transport.c:1417             | d0 | main                     | region_leave | r1  |  0.011541 |  0.008508 | transport_pu | ..label:get_refs_list
06:45:41.507368 run-command.c:740            | d0 | main                     | child_start  |     |  0.013092 |           |              | ......[ch1] class:? argv:[git pack-objects --all-progress-implied --revs --stdout]
06:45:41.539552 builtin/pack-objects.c:4808  | d1 | main                     | region_leave | r1  |  0.029332 |  0.028369 | pack-objects | label:enumerate-objects
06:45:41.540269 run-command.c:996            | d0 | main                     | child_exit   |     |  0.045990 |  0.032898 |              | ......[ch1] pid:16017 code:0
06:45:41.660000 run-command.c:996            | d0 | main                     | child_exit   |     |  0.170000 |  0.166949 |              | ....[ch0] pid:16016 code:0
";

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn git_perf_regions_and_children() {
        let phases = parse_git_perf(GIT_PERF);
        let get = |name: &str| {
            phases
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} in {phases:?}"))
        };
        assert!((get("run: git-receive-pack").ms - 166.949).abs() < 1e-6);
        assert!((get("run: git pack-objects").ms - 32.898).abs() < 1e-6);
        assert_eq!(get("pack-objects: enumerate-objects").depth, 1);
        assert!((get("transport_pu: get_refs_list").ms - 8.508).abs() < 1e-6);
        assert_eq!(phases[0].name, "run: git-receive-pack", "slowest first");
    }

    #[test]
    fn grit_trace_spans_sum_by_name() {
        let text = "grit.repository.open: new\n  grit.config.load: new\n  grit.config.load: close time.busy=211µs time.idle=0ns\ngrit.repository.open: close time.busy=1.50ms time.idle=0ns\ngrit.config.load: close time.busy=89µs time.idle=0ns\n";
        let phases = parse_grit_trace(text);
        assert_eq!(phases[0].name, "grit.repository.open");
        assert!((phases[0].ms - 1.5).abs() < 1e-9);
        let config = &phases[1];
        assert_eq!((config.count, config.depth), (2, 0));
        assert!((config.ms - 0.3).abs() < 1e-9);
    }

    #[test]
    fn display_argv_unwraps_shell_and_names_tools() {
        let grit = Path::new("/work/target/release/grit");
        let cmd = BenchArgv::new(
            "/bin/sh",
            [
                "-c",
                "set -e; '/work/target/release/grit' 'push' '-q' 'a b' > /dev/null 2>&1",
            ],
        );
        assert_eq!(display_argv(&cmd, &[(grit, "grit")]), "grit push -q 'a b'");
        let direct = BenchArgv::new("/usr/bin/git", ["status", "--short"]);
        assert_eq!(
            display_argv(&direct, &[(Path::new("/usr/bin/git"), "git")]),
            "git status --short"
        );
    }
}
