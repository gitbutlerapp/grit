//! Pipe long human output through a pager when stdout is a terminal.
//!
//! The pager command comes from `GRIT_PAGER`, then `core.pager`, then `PAGER`, then `less`
//! (Git's order, with `GRIT_PAGER` in place of `GIT_PAGER`). An empty value or `cat` turns
//! paging off. Like Git, `LESS=FRX` and `LV=-c` are set when the user has not set them, so a
//! short listing prints and exits instead of opening a full-screen view.

use std::io::{self, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use grit_lib::repo::Repository;

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
        let mut child = shell_command(&command)
            .stdin(Stdio::piped())
            .envs(default_env())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take();
        // Ctrl-C belongs to the pager (less uses it to stop a search); grit keeps writing
        // until the pager exits and the pipe closes.
        #[cfg(unix)]
        // SAFETY: `signal` only swaps this process's `SIGINT` disposition; `Drop` restores it.
        let previous_sigint = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        Some(Self {
            child,
            stdin,
            #[cfg(unix)]
            previous_sigint,
        })
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
        // SAFETY: restores the `SIGINT` disposition saved in `start`.
        unsafe {
            libc::signal(libc::SIGINT, self.previous_sigint);
        }
    }
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

/// Run `command` through the shell, so pager settings like `less -S` or `delta | less` work.
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
