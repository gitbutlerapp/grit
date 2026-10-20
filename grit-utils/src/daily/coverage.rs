//! MATRIX.md ↔ daily scenario registry coverage helpers.

use std::collections::{HashMap, HashSet};

use super::matrix_registry::MATRIX_SCENARIOS;
use super::registry::{ScenarioSpec, HEADLINE_SCENARIOS, REMOTE_MATRIX_SCENARIOS};

/// Every registered daily scenario descriptor, deduplicated by id (headlines first).
#[must_use]
pub fn all_registry_scenarios() -> Vec<&'static ScenarioSpec> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for spec in HEADLINE_SCENARIOS
        .iter()
        .chain(MATRIX_SCENARIOS.iter())
        .chain(REMOTE_MATRIX_SCENARIOS.iter())
    {
        if seen.insert(spec.id) {
            out.push(spec);
        }
    }
    out
}

/// Top-level grit subcommand name from a scenario id (`<command>/...`).
#[must_use]
pub fn scenario_command(id: &str) -> &str {
    id.split('/').next().unwrap_or(id)
}

/// Parse scenario ids listed in MATRIX.md table cells (`` `id` `` in the id column).
pub fn scenario_ids_in_matrix(markdown: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') || trimmed.starts_with("| ---") {
            continue;
        }
        let Some(cell) = trimmed
            .strip_prefix('|')
            .and_then(|rest| rest.trim().split('|').next())
        else {
            continue;
        };
        let cell = cell.trim();
        let Some(inner) = cell.strip_prefix('`').and_then(|s| s.strip_suffix('`')) else {
            continue;
        };
        if inner.contains('/') && inner.contains('@') {
            ids.push(inner.to_string());
        }
    }
    ids
}

/// Parse the Exemptions table: command name → reason (body before Exemptions ignored).
pub fn exemption_commands(markdown: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(section) = markdown.split("## Exemptions").nth(1) else {
        return map;
    };
    for line in section.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|')
            || trimmed.starts_with("| ---")
            || trimmed.starts_with("| Command")
        {
            continue;
        }
        let cells: Vec<_> = trimmed
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if cells.len() < 2 {
            continue;
        }
        let cmd = cells[0].trim_matches('`');
        if cmd.is_empty() || cmd == "Command" {
            continue;
        }
        map.insert(cmd.to_string(), cells[1].to_string());
    }
    map
}

/// Commands lacking both a registry scenario prefix and an exemption row.
pub fn uncovered_commands(
    top_level: impl IntoIterator<Item = impl AsRef<str>>,
    exemptions: &HashMap<String, String>,
    registry: &[&'static ScenarioSpec],
) -> Vec<String> {
    let covered: HashSet<String> = registry
        .iter()
        .map(|s| scenario_command(s.id).to_string())
        .collect();
    top_level
        .into_iter()
        .map(|c| c.as_ref().to_string())
        .filter(|c| !covered.contains(c) && !exemptions.contains_key(c))
        .collect()
}

/// Registry ids missing from MATRIX.md or listed more than once.
pub fn matrix_registry_gaps(
    registry: &[&'static ScenarioSpec],
    matrix_ids: &[String],
) -> (Vec<String>, Vec<String>) {
    let counts: HashMap<&str, usize> = matrix_ids.iter().fold(HashMap::new(), |mut acc, id| {
        *acc.entry(id.as_str()).or_default() += 1;
        acc
    });
    let mut missing = Vec::new();
    let mut duplicate = Vec::new();
    for spec in registry {
        match counts.get(spec.id).copied().unwrap_or(0) {
            0 => missing.push(spec.id.to_string()),
            n if n > 1 => duplicate.push(spec.id.to_string()),
            _ => {}
        }
    }
    (missing, duplicate)
}
