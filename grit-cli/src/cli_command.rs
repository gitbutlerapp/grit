//! Building the clap command tree: the tree `argv` is parsed with, plus the full tree for help
//! output, completions, and the tests' schema walks (large enough to overflow default
//! test-thread stacks).

use std::ffi::OsString;

use clap::CommandFactory;

use crate::Cli;

/// The command tree `argv` is parsed with.
///
/// Before parsing, clap copies every global argument of the root into each of its subcommands,
/// although only the invoked one is ever used. When `argv` names a subcommand, this gives that
/// subcommand the root's global arguments up front, exactly as clap would, and marks them
/// non-global on the root, so the other subcommands stay untouched. Parsing, matches and errors
/// for the invoked subcommand are unchanged. Without a known subcommand the tree is returned as
/// is, so suggestions for an unknown name still see every subcommand.
pub fn parse_command(argv: &[OsString]) -> clap::Command {
    let mut cmd = Cli::command();
    let Some(name) = crate::diff_args::subcommand_index(argv)
        .map(|i| argv[i].as_os_str())
        .filter(|name| cmd.find_subcommand(name).is_some())
    else {
        return cmd;
    };
    let globals: Vec<clap::Arg> = cmd
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .cloned()
        .collect();
    let Some(sub) = cmd.find_subcommand_mut(name) else {
        return cmd;
    };
    // Clap appends propagated globals before the subcommand's own (deferred) arguments, keeping
    // the root's display order, and the subcommand's own arguments still number from zero.
    *sub = std::mem::take(sub)
        .next_display_order(None)
        .args(globals)
        .next_display_order(0);
    cmd.mut_args(|arg| {
        if arg.is_global_set() {
            arg.global(false)
        } else {
            arg
        }
    })
}

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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use clap::{CommandFactory, Parser};

    use crate::Cli;

    /// What a parse produced, comparable across command trees.
    fn outcome(result: Result<Cli, clap::Error>) -> String {
        match result {
            Ok(cli) => format!("ok {cli:?}"),
            Err(err) => format!("err {:?} {}", err.kind(), err.render()),
        }
    }

    fn argv_corpus() -> Vec<Vec<String>> {
        let root = Cli::command();
        let mut corpus: Vec<Vec<String>> = vec![
            vec![],
            vec!["--json".into()],
            vec!["--markdown".into()],
            vec!["brnch".into()],
            vec!["--json".into(), "brnch".into()],
            vec!["-C".into(), "x".into(), "branch".into(), "--json".into()],
            vec!["--json".into(), "-C".into(), "x".into(), "status".into()],
            vec!["branch".into(), "-C".into(), "x".into()],
            vec!["--threads".into(), "two".into(), "branch".into()],
            vec!["branch".into(), "--threads".into(), "two".into()],
        ];
        for sub in root.get_subcommands() {
            let mut names = vec![sub.get_name().to_owned()];
            names.extend(sub.get_all_aliases().map(str::to_owned));
            for name in names {
                for (before, after) in [
                    (vec![], vec![]),
                    (vec![], vec!["--json"]),
                    (vec!["--json"], vec![]),
                    (vec!["--markdown"], vec!["--json"]),
                    (vec![], vec!["--json", "--markdown"]),
                    (vec![], vec!["--json", "--filter", ".x", "--threads", "2"]),
                    (vec!["--threads", "1"], vec!["--markdown"]),
                    (vec![], vec!["--no-such-flag"]),
                    (vec![], vec!["--filter"]),
                    (vec![], vec!["--help"]),
                    (vec![], vec!["-h"]),
                ] {
                    let mut argv: Vec<String> = before.into_iter().map(str::to_owned).collect();
                    argv.push(name.clone());
                    argv.extend(after.into_iter().map(str::to_owned));
                    corpus.push(argv);
                }
                for nested in sub.get_subcommands() {
                    corpus.push(vec![
                        name.clone(),
                        nested.get_name().to_owned(),
                        "--json".into(),
                    ]);
                    corpus.push(vec![
                        "--markdown".into(),
                        name.clone(),
                        nested.get_name().to_owned(),
                        "--no-such-flag".into(),
                    ]);
                }
            }
        }
        corpus
    }

    #[test]
    fn parse_command_matches_the_stock_tree_for_every_subcommand() {
        crate::cli_test_stack::run(|| {
            for args in argv_corpus() {
                let argv: Vec<OsString> = std::iter::once("grit".to_owned())
                    .chain(args)
                    .map(OsString::from)
                    .collect();
                let stock = outcome(Cli::try_parse_from(&argv));
                let parsed = outcome(crate::parse_cli(&argv));
                assert_eq!(parsed, stock, "argv {argv:?}");
            }
        });
    }

    #[test]
    fn parse_command_leaves_other_subcommands_without_globals() {
        crate::cli_test_stack::run(|| {
            let argv: Vec<OsString> = ["grit", "--json", "branch"].map(OsString::from).to_vec();
            let mut cmd = super::parse_command(&argv);
            cmd.build();
            let global_ids = ["json", "markdown", "filter", "threads"];
            for sub in cmd.get_subcommands() {
                let has_globals = global_ids
                    .iter()
                    .all(|id| sub.get_arguments().any(|arg| arg.get_id() == *id));
                assert_eq!(
                    has_globals,
                    sub.get_name() == "branch",
                    "{}",
                    sub.get_name()
                );
            }
            assert!(cmd.get_arguments().all(|arg| !arg.is_global_set()));
        });
    }

    #[test]
    fn parse_command_keeps_the_stock_tree_without_a_known_subcommand() {
        crate::cli_test_stack::run(|| {
            for args in [vec!["grit"], vec!["grit", "--json"], vec!["grit", "brnch"]] {
                let argv: Vec<OsString> = args.into_iter().map(OsString::from).collect();
                let cmd = super::parse_command(&argv);
                let globals = cmd
                    .get_arguments()
                    .filter(|arg| arg.is_global_set())
                    .count();
                assert_eq!(globals, 4, "argv {argv:?}");
            }
        });
    }
}
