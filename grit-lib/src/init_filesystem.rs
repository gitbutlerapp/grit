//! Filesystem capability probes during repository initialization.
//!
//! Git sets `core.ignorecase` and `core.precomposeunicode` in the new local config when
//! initialization detects a case-insensitive or NFD/NFC-aliasing filesystem.

use std::path::Path;

use crate::config::{parse_config_parameters, ConfigFile, ConfigScope, ConfigSet};
use crate::error::Result;
use crate::unicode_normalization::probe_filesystem_normalizes_nfd_to_nfc;

/// Controls when init-time filesystem probes may write local config.
#[derive(Debug, Clone, Copy, Default)]
pub struct InitFilesystemConfigOptions {
    /// Skip all probes (Git skips on re-init).
    pub is_reinit: bool,
}

fn precompose_from_git_config_parameters() -> Option<bool> {
    let Ok(raw) = std::env::var("GIT_CONFIG_PARAMETERS") else {
        return None;
    };
    let mut last: Option<bool> = None;
    for entry in parse_config_parameters(&raw) {
        let Some((k, v)) = entry.split_once('=') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("core.precomposeunicode") {
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

fn ignorecase_from_git_config_parameters() -> Option<bool> {
    let Ok(raw) = std::env::var("GIT_CONFIG_PARAMETERS") else {
        return None;
    };
    let mut last: Option<bool> = None;
    for entry in parse_config_parameters(&raw) {
        let Some((k, v)) = entry.split_once('=') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("core.ignorecase") {
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

/// After a fresh `.git` layout is written, probe the filesystem and set local
/// `core.precomposeunicode` / `core.ignorecase` when appropriate.
///
/// Honors higher-priority config (system, global, `GIT_CONFIG_PARAMETERS`), values already
/// present in the local config (including from an init template), and does not override them.
/// `GIT_TEST_UTF8_NFD_TO_NFC=1` forces the precompose probe on Linux CI only when the key is
/// not already set locally.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] or config parse/write failures.
pub fn apply_init_filesystem_config(
    git_dir: &Path,
    opts: InitFilesystemConfigOptions,
) -> Result<()> {
    if opts.is_reinit {
        return Ok(());
    }

    let protected = ConfigSet::load_protected(true)?;
    let precompose_externally = protected.get("core.precomposeunicode").is_some()
        || precompose_from_git_config_parameters().is_some();
    let ignorecase_externally = protected.get("core.ignorecase").is_some()
        || ignorecase_from_git_config_parameters().is_some();

    let force_precompose_probe = matches!(
        std::env::var("GIT_TEST_UTF8_NFD_TO_NFC").ok().as_deref(),
        Some("true") | Some("1")
    );

    let mut changed = false;
    let config_path = git_dir.join("config");
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
    let precompose_locally_set = cfg.get("core.precomposeunicode").is_some();
    let ignorecase_locally_set = cfg.get("core.ignorecase").is_some();

    if !precompose_externally && !precompose_locally_set {
        let probe_ok = force_precompose_probe
            || probe_filesystem_normalizes_nfd_to_nfc(git_dir).unwrap_or(false);
        if probe_ok {
            cfg.set("core.precomposeunicode", "true")?;
            changed = true;
        }
    }

    if !ignorecase_externally
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
        apply_init_filesystem_config(&git_dir, InitFilesystemConfigOptions::default())
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
}
