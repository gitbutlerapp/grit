//! Building the clap command tree for tests and schema walks (large enough to overflow default stacks).

use clap::CommandFactory;

use crate::Cli;

const CLI_BUILD_STACK: usize = 32 * 1024 * 1024;

/// Build [`Cli::command()`] on a thread with extra stack space.
pub fn build_cli_command() -> clap::Command {
    std::thread::Builder::new()
        .stack_size(CLI_BUILD_STACK)
        .spawn(|| Cli::command())
        .expect("spawn build_cli_command thread")
        .join()
        .expect("join build_cli_command thread")
}
