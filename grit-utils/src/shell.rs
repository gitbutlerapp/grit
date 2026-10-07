//! Shell-safe command formatting for hyperfine.

use std::path::Path;

/// Quote a string for POSIX `sh -c` single-quoted strings.
pub fn shell_quote(s: &str) -> String {
    if s.contains('\'') {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    } else {
        format!("'{s}'")
    }
}

/// Build `program arg1 arg2` with every segment quoted.
pub fn shell_command(program: &Path, args: &[impl AsRef<str>]) -> String {
    let mut parts = vec![shell_quote(&program.to_string_lossy())];
    for arg in args {
        parts.push(shell_quote(arg.as_ref()));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn shell_command_quotes_program_and_args() {
        let cmd = shell_command(Path::new("/usr/bin/git"), &["add", "-A"]);
        assert_eq!(cmd, "'/usr/bin/git' 'add' '-A'");
    }
}
