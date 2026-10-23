<<<<<<< New base: fix(rebase): address --exec review and maintainer steering
//! Building the clap command tree for tests and schema walks (large enough to overflow default stacks).

use clap::CommandFactory;

use crate::Cli;

const CLI_BUILD_STACK: usize = 32 * 1024 * 1024;

fn build_on_large_stack() -> clap::Command {
    Cli::command()
}

/// Build [`Cli::command()`] on a thread with extra stack space.
pub fn build_cli_command() -> clap::Command {
    std::thread::Builder::new()
        .stack_size(CLI_BUILD_STACK)
        .spawn(build_on_large_stack)
        .ok()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_else(|| Cli::command())
}
|||||||
=======
//! Building the clap command tree for tests and schema walks (large enough to overflow default stacks).

use clap::CommandFactory;

use crate::Cli;

const CLI_BUILD_STACK: usize = 32 * 1024 * 1024;

fn build_on_large_stack() -> clap::Command {
    Cli::command()
}

/// Build [`Cli::command()`] on a thread with extra stack space.
pub fn build_cli_command() -> clap::Command {
    std::thread::Builder::new()
        .stack_size(CLI_BUILD_STACK)
        .spawn(build_on_large_stack)
        .ok()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_else(|| Cli::command())
}
>>>>>>> Current commit: fix(cli): build clap command tree on large-stack thread in tests
