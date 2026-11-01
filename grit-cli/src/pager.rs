//! Pipe long human output through a pager when stdout is a terminal.
//!
//! The pager command comes from `GRIT_PAGER`, then `core.pager`, then `PAGER`, then `less`
//! (Git's order, with `GRIT_PAGER` in place of `GIT_PAGER`). An empty value or `cat` turns
//! paging off. Like Git, `LESS=FRX` and `LV=-c` are set when the user has not set them, so a
//! short listing prints and exits instead of opening a full-screen view.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use grit_lib::repo::Repository;
use shell_words::split as shell_split;

/// A running pager; output written here appears in it.
///
/// Dropping it closes the pager's input and waits for the user to quit it.
pub struct Pager {
    child: Child,
    stdin: Option<ChildStdin>,
    #[cfg(unix)]
    previous_sigint: libc::sighandler_t,
}

impl Pager {
    /// Start the configured pager, or return `None` when paging is off or it cannot start.
    ///
    /// `repo` supplies `core.pager`; pass `None` outside a repository.
    pub fn start(repo: Option<&Repository>) -> Option<Self> {
        let command = pager_command(repo)?;
        try_start_pager(&command)
    }
}

impl Write for Pager {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.stdin {
            Some(stdin) => stdin.write(buf),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.stdin {
            Some(stdin) => stdin.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
        #[cfg(unix)]
        // SAFETY: restores the `SIGINT` disposition saved in `try_start_pager`.
        unsafe {
            libc::signal(libc::SIGINT, self.previous_sigint);
        }
    }
}

fn try_start_pager(command: &str) -> Option<Pager> {
    let env = default_env();
    let mut child = if needs_shell(command) {
        shell_command(command)
            .stdin(Stdio::piped())
            .envs(env.iter().copied())
            .spawn()
            .ok()?
    } else {
        spawn_simple_pager(command, &env)?
    };

    if child_exited_unsuccessfully(&mut child) {
        let _ = child.wait();
        pager_failed(command);
        return None;
    }

    let stdin = child.stdin.take();
    #[cfg(unix)]
    // Ctrl-C belongs to the pager (less uses it to stop a search); grit keeps writing
    // until the pager exits and the pipe closes.
    // SAFETY: `signal` only swaps this process's `SIGINT` disposition; `Drop` restores it.
    let previous_sigint = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    Some(Pager {
        child,
        stdin,
        #[cfg(unix)]
        previous_sigint,
    })
}

fn spawn_simple_pager(command: &str, env: &[(&'static str, &'static str)]) -> Option<Child> {
    let args = shell_split(command).ok()?;
    let (program, rest) = args.split_first()?;
    if program.is_empty() {
        return None;
    }

    #[cfg(unix)]
    if !program.contains('/') && find_in_path(program).is_none() {
        pager_failed(command);
        return None;
    }

    let mut cmd = Command::new(program);
    cmd.args(rest)
        .stdin(Stdio::piped())
        .envs(env.iter().copied());
    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            pager_failed(command);
            None
        }
        Err(_) => None,
    }
}

fn child_exited_unsuccessfully(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(Some(status)) if !status.success())
}

fn pager_failed(command: &str) {
    eprintln!("grit could not run the pager ({command}); writing log to the terminal instead");
}

/// Whether `command` needs a shell (`delta | less`, redirection, …).
fn needs_shell(command: &str) -> bool {
    command.as_bytes().iter().any(|&b| {
        matches!(
            b,
            b'|' | b'&' | b';' | b'>' | b'<' | b'(' | b')' | b'$' | b'\'' | b'"' | b'`' | b'\n'
        )
    })
}

/// The pager command line to run, or `None` when paging is turned off.
fn pager_command(repo: Option<&Repository>) -> Option<String> {
    let from_env = |name: &str| std::env::var(name).ok();
    let from_config = || {
        repo.and_then(|repo| repo.config().ok())
            .and_then(|config| config.get("core.pager"))
    };
    let command = from_env("GRIT_PAGER")
        .or_else(from_config)
        .or_else(|| from_env("PAGER"))
        .unwrap_or_else(|| "less".to_owned());
    let command = command.trim();
    (!command.is_empty() && command != "cat").then(|| command.to_owned())
}

/// `LESS` / `LV` defaults for the pager process, skipping any the user already set.
fn default_env() -> Vec<(&'static str, &'static str)> {
    [("LESS", "FRX"), ("LV", "-c")]
        .into_iter()
        .filter(|(name, _)| std::env::var_os(name).is_none())
        .collect()
}

/// First regular file named `tool` on `$PATH`.
#[cfg(unix)]
fn find_in_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|cand| cand.is_file())
}

/// Run `command` through the shell, so pager settings like `delta | less` work.
#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_shell_detects_pipes_and_redirection() {
        assert!(!needs_shell("less"));
        assert!(!needs_shell("less -S"));
        assert!(needs_shell("delta | less"));
        assert!(needs_shell("less > /dev/null"));
    }
}
