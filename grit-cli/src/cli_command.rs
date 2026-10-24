//! Building the clap command tree outside of parsing: help output, completions, and the tests'
//! schema walks (large enough to overflow default test-thread stacks).

use clap::CommandFactory;

use crate::Cli;

#[cfg(test)]
const CLI_BUILD_STACK: usize = 32 * 1024 * 1024;

#[cfg(test)]
fn build_on_large_stack() -> clap::Command {
    Cli::command()
}

/// Build [`Cli::command()`] on a thread with extra stack space.
#[cfg(test)]
pub fn build_cli_command() -> clap::Command {
    std::thread::Builder::new()
        .stack_size(CLI_BUILD_STACK)
        .spawn(build_on_large_stack)
        .ok()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_else(|| Cli::command())
}

/// The whole command tree, built as if no subcommand were deferred, for help output and
/// completions (not for parsing).
///
/// Clap builds a deferred subcommand after its parent, which differs from an eager build in
/// three visible ways. The parent's global arguments land before the subcommand's own. The
/// parent's `help` subtree misses nested subcommands. And the doc comment of a flattened
/// argument group or nested subcommand enum replaces the subcommand's own description.
/// Expanding each top-level subcommand first, with the parent's globals, names and description
/// in place, then building the root, gives the eager tree.
///
/// `bin_name` is the name usage lines show for the binary.
pub fn full_cli_command(bin_name: &str) -> clap::Command {
    let mut cmd = Cli::command().bin_name(bin_name);
    let root_name = cmd.get_name().to_owned();
    let globals: Vec<clap::Arg> = cmd
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .cloned()
        .collect();
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect();
    for name in names {
        cmd = cmd.mut_subcommand(&name, |sub| {
            let about = sub.get_about().cloned();
            let long_about = sub.get_long_about().cloned();
            // Globals keep the root's display order, and the subcommand's own arguments still
            // number from zero, as when the root propagates them.
            let mut sub = sub
                .next_display_order(None)
                .args(globals.iter().cloned())
                .next_display_order(0)
                .bin_name(format!("{bin_name} {name}"))
                .display_name(format!("{root_name}-{name}"));
            sub.build();
            let sub = match about {
                Some(about) => sub.about(about),
                None => sub.about(None::<&str>),
            };
            match long_about {
                Some(long_about) => sub.long_about(long_about),
                None => sub.long_about(None::<&str>),
            }
        });
    }
    cmd.build();
    cmd
}
