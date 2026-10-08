//! Filesystem capability probes during repository initialization.
//!
//! Git sets `core.filemode`, `core.symlinks`, `core.ignorecase`, and
//! `core.precomposeunicode` in the new local config when initialization detects
//! platform limits.

use std::fs;
use std::path::Path;

use crate::config::{parse_config_parameters, ConfigFile, ConfigScope};
use crate::environment::Environment;
use crate::error::Result;
use crate::unicode_normalization::probe_filesystem_normalizes_nfd_to_nfc;

/// Controls when init-time filesystem probes may write local config.
#[derive(Debug, Clone, Copy, Default)]
pub struct InitFilesystemConfigOptions {
    /// Skip all probes (Git skips on re-init).
    pub is_reinit: bool,
}

fn config_bool_from_parameters(env: &Environment, key: &str) -> Option<bool> {
    let raw = env.git_config_parameters.as_deref()?;
    let key_lower = key.to_ascii_lowercase();
    let mut last: Option<bool> = None;
    for entry in parse_config_parameters(raw) {
        let Some((k, v)) = entry.split_once('=') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case(&key_lower) {
            continue;
        }
        let v = v.trim();
        last = Some(matches!(
            v.to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1"
        ));
    }
    last
}

fn precompose_from_git_config_parameters(env: &Environment) -> Option<bool> {
    config_bool_from_parameters(env, "core.precomposeunicode")
}

fn ignorecase_from_git_config_parameters(env: &Environment) -> Option<bool> {
    config_bool_from_parameters(env, "core.ignorecase")
}

fn filemode_from_git_config_parameters(env: &Environment) -> Option<bool> {
    config_bool_from_parameters(env, "core.filemode")
}

fn symlinks_from_git_config_parameters(env: &Environment) -> Option<bool> {
    config_bool_from_parameters(env, "core.symlinks")
}

/// True when `config` in `git_dir` is visible as `CoNfIg` (case-insensitive git dir).
///
/// Matches Git `init_db` / `setup.c` case-insensitivity detection.
pub fn probe_filesystem_case_insensitive(git_dir: &Path) -> std::io::Result<bool> {
    let config_path = git_dir.join("config");
    if !config_path.is_file() {
        return Ok(false);
    }
    Ok(git_dir.join("CoNfIg").is_file())
}

/// Probe whether the executable bit is meaningful for `git_dir/config`.
///
/// Matches Git `init_db` filemode trustability check in `setup.c`.
pub fn probe_trust_filemode(git_dir: &Path) -> std::io::Result<bool> {
    let config_path = git_dir.join("config");
    if !config_path.is_file() {
        return Ok(true);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(&config_path)?;
        let mode = meta.permissions().mode();
        let toggled = mode ^ 0o100;
        fs::set_permissions(&config_path, fs::Permissions::from_mode(toggled))?;
        let meta2 = fs::metadata(&config_path)?;
        let mut filemode = mode != meta2.permissions().mode();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(mode))?;
        if filemode && mode & 0o100 != 0 {
            filemode = false;
        }
        Ok(filemode)
    }

    #[cfg(not(unix))]
    {
        let _ = git_dir;
        Ok(false)
    }
}

/// Probe whether symbolic links can be created under `git_dir`.
///
/// Uses a unique probe path and never deletes pre-existing files (e.g. from init templates).
pub fn probe_symlinks_supported(git_dir: &Path) -> std::io::Result<bool> {
    for attempt in 0..128_u32 {
        let probe_path = git_dir.join(format!(".grit-symlink-probe-{attempt:05}"));
        if fs::symlink_metadata(&probe_path).is_ok() {
            continue;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            return Ok(if symlink("testing", &probe_path).is_err() {
                false
            } else {
                let ok = fs::symlink_metadata(&probe_path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                let _ = fs::remove_file(&probe_path);
                ok
            });
        }

        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_file;
            return Ok(if symlink_file("testing", &probe_path).is_err() {
                false
            } else {
                let ok = fs::symlink_metadata(&probe_path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                let _ = fs::remove_file(&probe_path);
                ok
            });
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = git_dir;
            return Ok(false);
        }
    }
    Ok(false)
}

/// After a fresh `.git` layout is written, probe the filesystem and set local
/// `core.*` keys when appropriate.
///
/// Honors higher-priority config (system, global, `GIT_CONFIG_PARAMETERS`), values already
/// present in the local config (including from an init template), and does not override them.
/// `GIT_TEST_UTF8_NFD_TO_NFC=1` forces the precompose probe on Linux CI only when the key is
/// not already set locally.
///
/// # Errors
///
/// Returns [`Error::Io`](crate::error::Error::Io) or config parse/write failures.
pub fn apply_init_filesystem_config(
    git_dir: &Path,
    opts: InitFilesystemConfigOptions,
    environment: &Environment,
) -> Result<()> {
    if opts.is_reinit {
        return Ok(());
    }

    let precompose_from_cmdline = precompose_from_git_config_parameters(environment).is_some();
    let ignorecase_from_cmdline = ignorecase_from_git_config_parameters(environment).is_some();
    let filemode_from_cmdline = filemode_from_git_config_parameters(environment).is_some();
    let symlinks_from_cmdline = symlinks_from_git_config_parameters(environment).is_some();

    let force_precompose_probe = environment
        .git_test_utf8_nfd_to_nfc
        .as_deref()
        .is_some_and(|v| v == "true" || v == "1");

    let filemode = probe_trust_filemode(git_dir).unwrap_or(true);
    let symlinks = probe_symlinks_supported(git_dir).unwrap_or(false);

    let mut changed = false;
    let config_path = git_dir.join("config");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
    let precompose_locally_set = cfg.get("core.precomposeunicode").is_some();
    let ignorecase_locally_set = cfg.get("core.ignorecase").is_some();

    if !filemode_from_cmdline {
        cfg.set("core.filemode", if filemode { "true" } else { "false" })?;
        changed = true;
    }

    if !symlinks_from_cmdline && !symlinks {
        cfg.set("core.symlinks", "false")?;
        changed = true;
    }

    if !precompose_from_cmdline && !precompose_locally_set {
        let probe_ok = force_precompose_probe
            || probe_filesystem_normalizes_nfd_to_nfc(git_dir).unwrap_or(false);
        if probe_ok {
            cfg.set("core.precomposeunicode", "true")?;
            changed = true;
        }
    }

    if !ignorecase_from_cmdline
        && !ignorecase_locally_set
        && probe_filesystem_case_insensitive(git_dir).unwrap_or(false)
    {
        cfg.set("core.ignorecase", "true")?;
        changed = true;
    }

    if changed {
        cfg.write()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn case_insensitive_probe_false_on_normal_fs() {
        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(git_dir.join("config"), "[core]\n").expect("config");
        assert!(!probe_filesystem_case_insensitive(&git_dir).expect("probe"));
    }

    #[test]
    fn apply_sets_filemode_from_probe() {
        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n",
        )
        .expect("config");
        apply_init_filesystem_config(
            &git_dir,
            InitFilesystemConfigOptions::default(),
            &Environment::capture_process(),
        )
        .expect("apply");
        let probed = probe_trust_filemode(&git_dir).expect("probe");
        let text = fs::read_to_string(git_dir.join("config")).expect("read");
        let expected = if probed {
            "filemode = true"
        } else {
            "filemode = false"
        };
        assert!(text.contains(expected), "{text}");
    }

    #[test]
    fn apply_respects_git_test_utf8_nfd_to_nfc() {
        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("config");
        std::env::set_var("GIT_TEST_UTF8_NFD_TO_NFC", "1");
        apply_init_filesystem_config(
            &git_dir,
            InitFilesystemConfigOptions::default(),
            &Environment::capture_process(),
        )
        .expect("apply");
        std::env::remove_var("GIT_TEST_UTF8_NFD_TO_NFC");
        let text = fs::read_to_string(git_dir.join("config")).expect("read");
        assert!(text.contains("precomposeunicode = true"));
    }

    #[test]
    fn apply_does_not_override_template_precomposeunicode_false() {
        let tmpl = TempDir::new().expect("tempdir");
        fs::write(
            tmpl.path().join("config"),
            "[core]\n\tprecomposeunicode = false\n",
        )
        .expect("template config");

        let root = TempDir::new().expect("worktree");
        std::env::set_var("GIT_TEST_UTF8_NFD_TO_NFC", "1");
        crate::repo::init_repository(root.path(), false, "main", Some(tmpl.path()), "files")
            .expect("init");
        std::env::remove_var("GIT_TEST_UTF8_NFD_TO_NFC");

        let text = fs::read_to_string(root.path().join(".git/config")).expect("read");
        assert!(
            text.contains("precomposeunicode = false"),
            "template local value must be preserved:\n{text}"
        );
    }

    #[test]
    fn symlink_probe_does_not_delete_template_file() {
        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(git_dir.join("config"), "[core]\n").expect("config");
        let sentinel = git_dir.join(".grit-symlink-probe-00000");
        fs::write(&sentinel, "keep me").expect("sentinel");
        let _ = probe_symlinks_supported(&git_dir);
        assert!(
            sentinel.is_file(),
            "probe must not remove an existing template file"
        );
        assert_eq!(fs::read_to_string(&sentinel).expect("read"), "keep me");
    }
}
