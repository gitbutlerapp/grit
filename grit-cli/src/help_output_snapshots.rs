//! Help text snapshots from [`crate::cli_command::full_cli_command`], proving the clap tree stays fixed across refactors.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use crate::cli_command::full_cli_command;
use crate::cli_test_stack;

fn snapshot_dir() -> &'static std::path::Path {
    std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"))
}

/// Long help for `cmd`, including usage and options (not `--version` output).
fn render_long_help(cmd: &clap::Command) -> String {
    let mut copy = cmd.clone();
    copy.render_long_help().to_string()
}

fn collect_help(cmd: &clap::Command, path: &[String], out: &mut BTreeMap<String, String>) {
    let key = if path.is_empty() {
        "root".to_owned()
    } else {
        path.join(" ")
    };
    out.insert(key, render_long_help(cmd));
    for sub in cmd.get_subcommands() {
        let mut next = path.to_vec();
        next.push(sub.get_name().to_owned());
        collect_help(sub, &next, out);
    }
}

/// Every subcommand path's `--help` text, plus the root listing.
#[test]
fn help_output_snapshots() {
    cli_test_stack::run(|| {
        let root = full_cli_command("grit");
        let mut helps = BTreeMap::new();
        collect_help(&root, &[], &mut helps);
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path(snapshot_dir());
        settings.set_prepend_module_to_snapshot(false);
        settings.bind(|| {
            insta::assert_json_snapshot!("grit_test_support__cli_fixture__help_output", helps);
        });
    });
}
