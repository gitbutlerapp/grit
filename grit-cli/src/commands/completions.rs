//! `grit completions` — emit shell completion scripts for the current CLI.

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{generate, Shell};

use crate::Cli;

/// Write a completion script for `shell` to stdout.
pub fn run(shell: Shell) -> Result<()> {
    generate(shell, &mut Cli::command(), "grit", &mut std::io::stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const COMPLETION_TEST_STACK: usize = 32 * 1024 * 1024;

    fn script_for(shell: Shell) -> String {
        std::thread::Builder::new()
            .stack_size(COMPLETION_TEST_STACK)
            .spawn(move || {
                let mut buf = Cursor::new(Vec::new());
                generate(
                    shell,
                    &mut crate::cli_command::build_cli_command(),
                    "grit",
                    &mut buf,
                );
                String::from_utf8(buf.into_inner()).expect("completion script is utf-8")
            })
            .expect("spawn completion generator thread")
            .join()
            .expect("join completion generator thread")
    }

    #[test]
    fn bash_includes_core_commands_and_json_flag() {
        let script = script_for(Shell::Bash);
        assert!(script.contains("status"));
        assert!(script.contains("commit"));
        assert!(script.contains("fetch"));
        assert!(script.contains("--json"));
    }

    #[test]
    fn zsh_includes_core_commands_and_json_flag() {
        let script = script_for(Shell::Zsh);
        assert!(script.contains("status"));
        assert!(script.contains("commit"));
        assert!(script.contains("fetch"));
        assert!(script.contains("--json"));
    }

    #[test]
    fn fish_includes_core_commands_and_json_flag() {
        let script = script_for(Shell::Fish);
        assert!(script.contains("status"));
        assert!(script.contains("commit"));
        assert!(script.contains("fetch"));
        assert!(script.contains("--json"));
    }
}
