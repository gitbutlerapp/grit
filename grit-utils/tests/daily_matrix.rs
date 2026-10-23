//! MATRIX.md coverage: every `grit` subcommand and every registry scenario id.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use grit_utils::daily::areas;
use grit_utils::daily::coverage::{
    all_registry_scenarios, exemption_commands, matrix_registry_gaps, scenario_command,
    scenario_ids_in_matrix, uncovered_commands,
};

fn matrix_md() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("MATRIX.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn grit_top_level_commands() -> Vec<String> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let grit = workspace.join("target/debug/grit");
    if !grit.is_file() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "grit-cli", "--bin", "grit", "-q"])
            .current_dir(&workspace)
            .status()
            .expect("build grit");
        assert!(status.success(), "cargo build grit-cli failed");
    }
    let out = Command::new(&grit)
        .arg("--help")
        .output()
        .expect("grit --help");
    assert!(
        out.status.success(),
        "grit --help failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut commands = Vec::new();
    let mut in_commands = false;
    for line in text.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            if line.trim().is_empty() || line.starts_with("Options:") {
                break;
            }
            let Some(name) = line.trim().split_whitespace().next() else {
                continue;
            };
            if name == "help" {
                continue;
            }
            commands.push(name.to_string());
        }
    }
    commands.sort();
    commands.dedup();
    commands
}

#[test]
fn daily_matrix_covers_every_command() {
    let md = matrix_md();
    let exemptions = exemption_commands(&md);
    let registry = all_registry_scenarios();
    let commands = grit_top_level_commands();
    let cmd_names: Vec<&str> = commands.iter().map(String::as_str).collect();
    let missing = uncovered_commands(&cmd_names, &exemptions, &registry);
    assert!(
        missing.is_empty(),
        "grit subcommands without a daily scenario or MATRIX.md exemption: {missing:?}\n\
         Add a registry row or an Exemptions table entry in grit-utils/MATRIX.md"
    );
}

#[test]
fn registry_scenarios_have_daily_plans() {
    let plan_ids: std::collections::HashSet<_> =
        areas::all_daily_plans().iter().map(|p| p.spec.id).collect();
    for spec in all_registry_scenarios() {
        assert!(
            plan_ids.contains(spec.id),
            "missing daily plan for registry id {}",
            spec.id
        );
    }
}

#[test]
fn matrix_lists_every_scenario_once() {
    let md = matrix_md();
    let matrix_ids = scenario_ids_in_matrix(&md);
    let registry = all_registry_scenarios();
    let (missing, duplicate) = matrix_registry_gaps(&registry, &matrix_ids);
    assert!(
        missing.is_empty(),
        "registry scenario ids missing from MATRIX.md: {missing:?}"
    );
    assert!(
        duplicate.is_empty(),
        "registry scenario ids listed more than once in MATRIX.md: {duplicate:?}"
    );
    assert_eq!(
        matrix_ids.len(),
        registry.len(),
        "MATRIX.md lists {} ids but registry has {}",
        matrix_ids.len(),
        registry.len()
    );
}

#[test]
fn matrix_coverage_logic_catches_dummy_injections() {
    let md = matrix_md();
    let exemptions = exemption_commands(&md);
    let registry = all_registry_scenarios();
    let commands = grit_top_level_commands();

    let mut dummy_registry: Vec<_> = registry.clone();
    dummy_registry.push(&grit_utils::daily::registry::ScenarioSpec {
        id: "dummy-never@small",
        area: grit_utils::daily::registry::Area::Worktree,
        headline: false,
        name: None,
        description: "injected for coverage test",
    });
    let matrix_ids = scenario_ids_in_matrix(&md);
    let (missing, _) = matrix_registry_gaps(&dummy_registry, &matrix_ids);
    assert!(
        missing.iter().any(|id| id == "dummy-never@small"),
        "expected injected scenario to be reported missing from MATRIX.md"
    );

    let mut dummy_exempt = exemptions.clone();
    dummy_exempt.remove("auth");
    let cmd_names: Vec<&str> = commands.iter().map(String::as_str).collect();
    let uncovered = uncovered_commands(&cmd_names, &dummy_exempt, &registry);
    assert!(
        uncovered.iter().any(|c| c == "auth"),
        "expected auth to be uncovered when exemption row removed"
    );

    let mut covered_cmds: HashSet<String> = registry
        .iter()
        .map(|s| scenario_command(s.id).to_string())
        .collect();
    covered_cmds.remove("status");
    let missing_cmd: Vec<String> = commands
        .iter()
        .filter(|c| **c == "status" && !dummy_exempt.contains_key(*c))
        .cloned()
        .collect();
    assert_eq!(missing_cmd, vec!["status".to_string()]);
    let _ = covered_cmds;
}
