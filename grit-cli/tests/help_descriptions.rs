//! Each command's help leads with the description the top-level listing shows for it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit_stdout(args: &[&str]) -> String {
    let output = Command::new(GRIT).args(args).output().expect("spawn grit");
    assert!(
        output.status.success(),
        "grit {args:?}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 stdout")
}

/// `(name, description start)` for every command row under `Commands:` in `grit --help`.
fn listed_commands() -> Vec<(String, String)> {
    let help = grit_stdout(&["--help"]);
    let mut rows = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if !line.starts_with("  ") {
            break;
        }
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        if rest.starts_with(' ') {
            continue;
        }
        let Some((name, description)) = rest.split_once(' ') else {
            continue;
        };
        let description = description.trim();
        let description = description
            .split_once(" [alias")
            .map_or(description, |(text, _)| text);
        if name == "help" || description.is_empty() {
            continue;
        }
        rows.push((name.to_owned(), description.to_owned()));
    }
    rows
}

#[test]
fn command_help_shows_the_listed_description() {
    let commands = listed_commands();
    assert!(
        commands.len() > 20,
        "expected the full command listing, got {commands:?}"
    );
    for (name, description) in &commands {
        for args in [vec![name.as_str(), "-h"], vec!["help", name.as_str()]] {
            let help = grit_stdout(&args);
            let first = help.lines().next().unwrap_or_default();
            assert!(
                first.starts_with(description.as_str()),
                "grit {args:?} starts with {first:?}, but `grit --help` lists {name} as {description:?}"
            );
        }
    }
}
