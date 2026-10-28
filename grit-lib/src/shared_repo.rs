//! Shared-repository permission helpers (`core.sharedRepository`, `--shared`).
//!
//! Mirrors Git's `git_config_perm`, `calc_shared_perm`, and `adjust_shared_perm` in `setup.c` /
//! `path.c`.

use crate::config::parse_bool;
use crate::diagnostics::{self, DiagnosticSink};
#[cfg(unix)]
use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Use the process umask for new files (Git `PERM_UMASK`).
pub const PERM_UMASK: i32 = 0;
const OLD_PERM_GROUP: i32 = 1;
const OLD_PERM_EVERYBODY: i32 = 2;
/// Group-writable layout (`--shared=group` / `1`).
pub const PERM_GROUP: i32 = 0o660;
/// World-readable/writable layout (`--shared=all` / `2`).
pub const PERM_EVERYBODY: i32 = 0o664;

/// Parse `core.sharedRepository` / `git init --shared=` like Git's `git_config_perm`.
///
/// Returns an error when an octal mode is given but the owner lacks read+write (Git `die`).
pub fn git_config_perm(var: &str, value: &str) -> Result<i32, String> {
    git_config_perm_with_diagnostics(var, value, None)
}

/// Like [`git_config_perm`] but may emit [`diagnostics::Warning::BadBooleanConfig`].
pub fn git_config_perm_with_diagnostics(
    var: &str,
    value: &str,
    diagnostics: Option<&dyn DiagnosticSink>,
) -> Result<i32, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("umask") {
        return Ok(PERM_UMASK);
    }
    if value.eq_ignore_ascii_case("group") {
        return Ok(PERM_GROUP);
    }
    if value.eq_ignore_ascii_case("all")
        || value.eq_ignore_ascii_case("world")
        || value.eq_ignore_ascii_case("everybody")
    {
        return Ok(PERM_EVERYBODY);
    }

    // Git: strtol(value, &endptr, 8); if *endptr != 0, fall through to bool.
    let (octal_prefix, rest) = split_octal_prefix(value);
    if rest.is_empty() && !octal_prefix.is_empty() {
        if let Ok(i) = i32::from_str_radix(octal_prefix, 8) {
            return Ok(match i {
                PERM_UMASK => PERM_UMASK,
                OLD_PERM_GROUP => PERM_GROUP,
                OLD_PERM_EVERYBODY => PERM_EVERYBODY,
                _ => {
                    if (i & 0o600) != 0o600 {
                        return Err(format!(
                            "problem with core.sharedRepository filemode value (0{i:o}).\n\
                             The owner of files must always have read and write permissions."
                        ));
                    }
                    -(i & 0o666)
                }
            });
        }
    }

    match parse_bool(value) {
        Ok(true) => Ok(PERM_GROUP),
        Ok(false) => Ok(PERM_UMASK),
        Err(_) => {
            if let Some(d) = diagnostics {
                d.warn(diagnostics::Warning::BadBooleanConfig {
                    key: var.to_owned(),
                    value: value.to_owned(),
                });
            }
            Ok(PERM_UMASK)
        }
    }
}

fn split_octal_prefix(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b'0'..=b'7') {
        i += 1;
    }
    (&s[..i], &s[i..])
}

/// Git's `calc_shared_perm` (`path.c`).
#[must_use]
pub fn calc_shared_perm(shared_repo: i32, mode: u32) -> u32 {
    let tweak = if shared_repo < 0 {
        (-shared_repo) as u32
    } else {
        shared_repo as u32
    };

    let mut new_mode = if shared_repo < 0 {
        (mode & !0o777) | tweak
    } else {
        mode | tweak
    };

    if mode & 0o200 == 0 {
        new_mode &= !0o222;
    }
    if mode & 0o100 != 0 {
        new_mode |= (new_mode & 0o444) >> 2;
    }

    new_mode
}

/// Git's `adjust_shared_perm` for a single path (`path.c`), using an explicit `shared_repo` value.
///
/// When `shared_repo` is zero, this is a no-op. Symlinks are skipped.
#[cfg(unix)]
pub fn adjust_shared_perm_path(shared_repo: i32, path: &Path) -> std::io::Result<()> {
    if shared_repo == 0 {
        return Ok(());
    }

    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }

    let old_mode = meta.permissions().mode();
    let mut new_mode = calc_shared_perm(shared_repo, old_mode);
    if meta.is_dir() {
        new_mode |= (new_mode & 0o444) >> 2;
        // Match Git: setgid on directories is tied to explicit filemode-style sharing
        // (`shared_repo < 0`), not the legacy positive `PERM_GROUP` / `PERM_EVERYBODY` tweaks.
        // Default `grit init` uses positive `PERM_GROUP` and should report `775` in `stat -c %a`
        // (t12660); `git init --shared=0660` uses a negative mask and needs `drwxrw[sx]---` (t1301).
        if shared_repo < 0 {
            const S_ISGID: u32 = 0o002000;
            if (new_mode & 0o060) != 0 {
                new_mode |= S_ISGID;
            }
        }
    }

    let new_perm = fs::Permissions::from_mode(new_mode & 0o7777);
    if (old_mode & 0o7777) != (new_mode & 0o7777) {
        fs::set_permissions(path, new_perm)?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn adjust_shared_perm_path(_shared_repo: i32, _path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Finalize a published pack sidecar (`.pack`, `.idx`, or `.rev`) like Git's `index-pack`.
///
/// Strips write permission using the process umask (`0444 & ~umask`), then applies
/// [`adjust_shared_perm_path`] when `core.sharedRepository` is set. No-op on non-Unix.
#[cfg(unix)]
pub fn finalize_pack_file_mode(shared_repo: i32, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let old_umask = unsafe { libc::umask(0) };
    unsafe {
        libc::umask(old_umask);
    }
    let mode = 0o444 & !old_umask;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    adjust_shared_perm_path(shared_repo, path)
}

#[cfg(not(unix))]
pub fn finalize_pack_file_mode(_shared_repo: i32, _path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Resolve `core.sharedRepository` from config text (merged set's string value).
pub fn shared_repository_from_config_value(raw: Option<&str>) -> Result<i32, String> {
    let Some(v) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(PERM_UMASK);
    };
    git_config_perm("core.sharedRepository", v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{CollectingDiagnostics, Warning};

    #[test]
    fn git_config_perm_parses_git_shared_variants() {
        assert_eq!(
            git_config_perm("core.sharedRepository", "umask").expect("umask"),
            PERM_UMASK
        );
        assert_eq!(
            git_config_perm("core.sharedRepository", "group").expect("group"),
            PERM_GROUP
        );
        for v in ["all", "world", "everybody", "ALL"] {
            assert_eq!(
                git_config_perm("core.sharedRepository", v).expect(v),
                PERM_EVERYBODY
            );
        }
        assert_eq!(
            git_config_perm("core.sharedRepository", "true").expect("true"),
            PERM_GROUP
        );
        assert_eq!(
            git_config_perm("core.sharedRepository", "false").expect("false"),
            PERM_UMASK
        );
        let neg = git_config_perm("core.sharedRepository", "0660").expect("0660");
        assert!(neg < 0);
        assert_eq!(
            git_config_perm("core.sharedRepository", "1").expect("1"),
            PERM_GROUP
        );
        assert_eq!(
            git_config_perm("core.sharedRepository", "2").expect("2"),
            PERM_EVERYBODY
        );
        assert!(git_config_perm("core.sharedRepository", "0444").is_err());
    }

    #[test]
    fn git_config_perm_bad_bool_emits_diagnostic() {
        let cap = CollectingDiagnostics::new();
        assert_eq!(
            git_config_perm_with_diagnostics("core.sharedRepository", "maybe", Some(&cap))
                .expect("fallback"),
            PERM_UMASK
        );
        assert!(cap
            .warnings()
            .iter()
            .any(|w| matches!(w, Warning::BadBooleanConfig { .. })));
    }

    #[test]
    fn split_octal_prefix_and_calc_shared_perm() {
        assert_eq!(split_octal_prefix("0660x"), ("0660", "x"));
        assert_eq!(split_octal_prefix("abc"), ("", "abc"));
        let mode = 0o100644;
        assert_eq!(calc_shared_perm(PERM_GROUP, mode) & 0o777, 0o664);
        let neg = git_config_perm("core.sharedRepository", "0660").expect("0660");
        let adjusted = calc_shared_perm(neg, mode);
        assert_ne!(adjusted & 0o777, 0);
        assert_eq!(calc_shared_perm(PERM_UMASK, mode), mode);
    }

    #[test]
    fn shared_repository_from_config_value_empty() {
        assert_eq!(
            shared_repository_from_config_value(None).expect("none"),
            PERM_UMASK
        );
        assert_eq!(
            shared_repository_from_config_value(Some("  ")).expect("blank"),
            PERM_UMASK
        );
        assert_eq!(
            shared_repository_from_config_value(Some("group")).expect("group"),
            PERM_GROUP
        );
    }

    #[cfg(unix)]
    #[test]
    fn finalize_pack_file_mode_respects_umask_and_shared_repo() -> std::io::Result<()> {
        let old = unsafe { libc::umask(0o022) };
        unsafe {
            libc::umask(old);
        }
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("pack");
        std::fs::write(&file, b"pack")?;
        finalize_pack_file_mode(PERM_UMASK, &file)?;
        assert_eq!(
            std::fs::metadata(&file)?.permissions().mode() & 0o777,
            0o444,
            "umask 022 → 0444"
        );

        let file660 = dir.path().join("pack660");
        std::fs::write(&file660, b"pack")?;
        let shared660 = git_config_perm("core.sharedRepository", "0660").expect("0660");
        finalize_pack_file_mode(shared660, &file660)?;
        assert_eq!(
            std::fs::metadata(&file660)?.permissions().mode() & 0o777,
            0o440,
            "shared 0660 → 0440"
        );

        let old077 = unsafe { libc::umask(0o077) };
        let file077 = dir.path().join("pack077");
        std::fs::write(&file077, b"pack")?;
        finalize_pack_file_mode(PERM_UMASK, &file077)?;
        assert_eq!(
            std::fs::metadata(&file077)?.permissions().mode() & 0o777,
            0o400,
            "umask 077 → 0400"
        );
        unsafe {
            libc::umask(old077);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn adjust_shared_perm_path_updates_file_mode() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("obj");
        std::fs::write(&file, b"x")?;
        let shared = git_config_perm("core.sharedRepository", "0660").expect("0660");
        adjust_shared_perm_path(shared, &file)?;
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub)?;
        adjust_shared_perm_path(shared, &sub)?;
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link)?;
        adjust_shared_perm_path(shared, &link)?;
        adjust_shared_perm_path(0, &file)?;
        Ok(())
    }
}
