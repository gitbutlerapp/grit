//! Cross-platform terminal capability detection for ANSI color output.
//!
//! On Unix, any terminal interprets ANSI SGR escape sequences, so colored output
//! is safe whenever stdout/stderr is a TTY. On Windows that is not guaranteed:
//! the console only renders `\x1b[..m` sequences when *virtual terminal
//! processing* is enabled for the stream. Modern consoles (Windows Terminal,
//! recent `conhost`, VS Code) support it but require a one-time
//! `SetConsoleMode` opt-in; legacy consoles do not support it at all and would
//! otherwise print the raw escape bytes as visible garbage.
//!
//! [`ansi_supported`] enables the mode once (caching the result) and reports
//! whether the console can actually display ANSI colors, so callers can fall back
//! to uncolored output on terminals that don't support it.

use std::io::IsTerminal;

/// Whether the current console can interpret ANSI escape sequences.
///
/// Always `true` on non-Windows platforms. On Windows this enables virtual
/// terminal processing for the standard output/error handles on first call and
/// caches whether that succeeded; legacy consoles without VT support return
/// `false`.
#[must_use]
pub fn ansi_supported() -> bool {
    #[cfg(not(windows))]
    {
        true
    }
    #[cfg(windows)]
    {
        windows_impl::ansi_supported()
    }
}

/// Whether stdout is a terminal that can display ANSI colors.
///
/// Combines a TTY check with [`ansi_supported`], so it is the right gate for an
/// `auto` color decision on stdout.
#[must_use]
pub fn stdout_supports_color() -> bool {
    std::io::stdout().is_terminal() && ansi_supported()
}

/// Whether stderr is a terminal that can display ANSI colors.
///
/// The stderr counterpart to [`stdout_supports_color`].
#[must_use]
pub fn stderr_supports_color() -> bool {
    std::io::stderr().is_terminal() && ansi_supported()
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// Enable VT processing once for stdout and stderr; cache whether the console
    /// understands ANSI. Enabling on either standard stream proves the console is
    /// VT-capable, but we try both so colored stdout *and* stderr render.
    pub fn ansi_supported() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            let out = enable_vt(STD_OUTPUT_HANDLE);
            let err = enable_vt(STD_ERROR_HANDLE);
            out || err
        })
    }

    /// Try to enable [`ENABLE_VIRTUAL_TERMINAL_PROCESSING`] on the given standard
    /// handle. Returns whether the stream is a VT-capable console afterwards.
    fn enable_vt(which: STD_HANDLE) -> bool {
        // SAFETY: all three calls take a handle owned by the process and only read
        // or write a local `CONSOLE_MODE`; failures are reported via return value.
        unsafe {
            let handle = GetStdHandle(which);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut mode: CONSOLE_MODE = 0;
            // Fails when the handle isn't a real console (e.g. redirected to a
            // pipe or file); in that case there is nothing to color.
            if GetConsoleMode(handle, &mut mode) == 0 {
                return false;
            }
            if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
                return true; // already enabled
            }
            SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }
}
