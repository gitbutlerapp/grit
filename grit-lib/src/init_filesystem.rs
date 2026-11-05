//! Filesystem capability detection during repository initialization.
//!
//! Git sets `core.filemode`, `core.symlinks`, `core.ignorecase`, and
//! `core.precomposeunicode` in the new local config when initialization detects
//! platform limits.

use std::fs;
use std::io;
use std::path::Path;

use thiserror::Error;

use crate::config::{parse_config_parameters, ConfigFile, ConfigScope};
use crate::environment::Environment;
use crate::error::Result;
use crate::unicode_normalization;

/// Failure while detecting filesystem capabilities under a git directory.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FilesystemCapabilitiesError {
    /// Underlying I/O failed while reading or probing the filesystem.
    #[error("filesystem capability detection failed: {0}")]
    Io(#[from] io::Error),
}

/// Result of [`FilesystemCapabilities`] detection.
pub type FilesystemCapabilitiesResult<T> = std::result::Result<T, FilesystemCapabilitiesError>;

/// When init may run the NFD→NFC precompose capability check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrecomposeDetection {
    /// Run the precompose check only when the platform would alias NFD and NFC (default).
    #[default]
    Platform,
    /// Always run the precompose check (embedder/test knob, not from process env).
    Always,
}

/// Controls when init-time filesystem capability detection may write local config.
#[derive(Debug, Clone, Copy, Default)]
pub struct InitFilesystemConfigOptions {
    /// Skip all detection (Git skips on re-init).
    pub is_reinit: bool,
    /// How aggressively to detect `core.precomposeunicode`.
    pub precompose_detection: PrecomposeDetection,
}

/// Platform limits Git records as `core.*` during init.
///
/// # Examples
///
/// ```
/// use grit_lib::config::init_filesystem::FilesystemCapabilities;
/// use std::fs;
/// use tempfile::TempDir;
///
/// let td = TempDir::new().expect("tempdir");
/// let git_dir = td.path().join(".git");
/// fs::create_dir_all(&git_dir).expect("mkdir");
/// fs::write(git_dir.join("config"), "[core]\n").expect("config");
///
/// let caps = FilesystemCapabilities::detect(&git_dir).expect("detect");
/// let only_case = FilesystemCapabilities::detect_case_insensitive(&git_dir).expect("case");
/// assert_eq!(only_case, caps.case_insensitive);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FilesystemCapabilities {
    /// Whether ref and path names are compared case-insensitively (`core.ignorecase`).
    pub case_insensitive: bool,
    /// Whether the executable bit on files is meaningful (`core.filemode`).
    pub trust_filemode: bool,
    /// Whether symbolic links can be created under the git directory (`core.symlinks`).
    pub symlinks: bool,
    /// Whether NFD and NFC spellings alias the same path (`core.precomposeunicode`).
    pub precomposes_unicode: bool,
}

impl FilesystemCapabilities {
    /// Detect all init-time filesystem capabilities under `git_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemCapabilitiesError::Io`] when a probe cannot read or touch the filesystem.
    pub fn detect(git_dir: &Path) -> FilesystemCapabilitiesResult<Self> {
        Ok(Self {
            case_insensitive: Self::detect_case_insensitive(git_dir)?,
            trust_filemode: Self::detect_trust_filemode(git_dir)?,
            symlinks: Self::detect_symlinks_supported(git_dir)?,
            precomposes_unicode: Self::detect_precomposes_unicode(git_dir)?,
        })
    }

    /// Whether `config` in `git_dir` is visible as `CoNfIg` (case-insensitive git dir).
    ///
    /// Matches Git `init_db` / `setup.c` case-insensitivity detection.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemCapabilitiesError::Io`] when the git directory cannot be read.
    pub fn detect_case_insensitive(git_dir: &Path) -> FilesystemCapabilitiesResult<bool> {
        let config_path = git_dir.join("config");
        if !config_path.is_file() {
            return Ok(false);
        }
        Ok(git_dir.join("CoNfIg").is_file())
    }

    /// Whether the executable bit is meaningful for `git_dir/config`.
    ///
    /// Matches Git `init_db` filemode trustability check in `setup.c`.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemCapabilitiesError::Io`] when permissions cannot be read or toggled.
    pub fn detect_trust_filemode(git_dir: &Path) -> FilesystemCapabilitiesResult<bool> {
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

    /// Whether symbolic links can be created under `git_dir`.
    ///
    /// Uses a unique path and never deletes pre-existing files (e.g. from init templates).
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemCapabilitiesError::Io`] when the probe path cannot be created.
    pub fn detect_symlinks_supported(git_dir: &Path) -> FilesystemCapabilitiesResult<bool> {
        for attempt in 0..128_u32 {
            let probe_path = git_dir.join(format!(".grit-symlink-capability-{attempt:05}"));
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

    /// Whether creating a file with an NFC filename makes the NFD spelling visible (macOS / HFS+ style).
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemCapabilitiesError::Io`] when the probe file cannot be written.
    pub fn detect_precomposes_unicode(git_dir: &Path) -> FilesystemCapabilitiesResult<bool> {
        unicode_normalization::filesystem_precomposes_unicode(git_dir).map_err(Into::into)
    }
}

fn config_bool_from_parameters(env: &Environment, key: &str) -> Option<bool> {
    let raw = env.git_config_parameters.as_deref()?;
    let key_lower = key.to_ascii_lowercase();
    let mut last: Option<bool> = None;
    for entry in parse_config_parameters(raw) {
        let Some((k, v)) = entry.split_once('\u{1}') else {
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

/// After a fresh `.git` layout is written, detect filesystem capabilities and set local
/// `core.*` keys when appropriate.
///
/// Honors higher-priority config (system, global, `GIT_CONFIG_PARAMETERS`), values already
/// present in the local config (including from an init template), and does not override them.
/// When [`InitFilesystemConfigOptions::precompose_detection`] is [`PrecomposeDetection::Always`],
/// the precompose check runs even on filesystems that would not normally normalize NFD to NFC
/// (Linux CI).
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

    let caps = FilesystemCapabilities::detect(git_dir).unwrap_or(FilesystemCapabilities {
        case_insensitive: false,
        trust_filemode: true,
        symlinks: false,
        precomposes_unicode: false,
    });

    let mut changed = false;
    let config_path = git_dir.join("config");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
    let precompose_locally_set = cfg.get("core.precomposeunicode").is_some();
    let ignorecase_locally_set = cfg.get("core.ignorecase").is_some();

    let filemode_value = filemode_from_git_config_parameters(environment)
        .map(|v| if v { "true" } else { "false" })
        .unwrap_or_else(|| if caps.trust_filemode { "true" } else { "false" });
    cfg.set("core.filemode", filemode_value)?;
    changed |= true;

    if let Some(from_cmdline) = symlinks_from_git_config_parameters(environment) {
        cfg.set("core.symlinks", if from_cmdline { "true" } else { "false" })?;
        changed |= true;
    } else if !caps.symlinks {
        cfg.set("core.symlinks", "false")?;
        changed |= true;
    }

    if !precompose_from_cmdline && !precompose_locally_set {
        let precompose_ok = matches!(opts.precompose_detection, PrecomposeDetection::Always)
            || caps.precomposes_unicode;
        if precompose_ok {
            cfg.set("core.precomposeunicode", "true")?;
            changed |= true;
        }
    }

    if !ignorecase_from_cmdline && !ignorecase_locally_set && caps.case_insensitive {
        cfg.set("core.ignorecase", "true")?;
        changed |= true;
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
    fn case_insensitive_detection_false_on_normal_fs() {
        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(git_dir.join("config"), "[core]\n").expect("config");
        assert!(!FilesystemCapabilities::detect_case_insensitive(&git_dir).expect("detect"));
    }

    #[test]
    fn apply_honors_git_config_parameters_filemode() {
        use crate::config::environment::ConfigOverride;

        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n",
        )
        .expect("config");
        let mut env = Environment::empty();
        env.push_config_override(
            &"core.filemode=false"
                .parse::<ConfigOverride>()
                .expect("override"),
        );
        apply_init_filesystem_config(&git_dir, InitFilesystemConfigOptions::default(), &env)
            .expect("apply");
        let text = fs::read_to_string(git_dir.join("config")).expect("read");
        assert!(text.contains("filemode = false"), "{text}");
    }

    #[test]
    fn apply_sets_filemode_from_detection() {
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
            &crate::environment::Environment::empty(),
        )
        .expect("apply");
        let detected = FilesystemCapabilities::detect_trust_filemode(&git_dir).expect("detect");
        let text = fs::read_to_string(git_dir.join("config")).expect("read");
        let expected = if detected {
            "filemode = true"
        } else {
            "filemode = false"
        };
        assert!(text.contains(expected), "{text}");
    }

    #[test]
    fn apply_respects_precompose_detection_always() {
        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("config");
        apply_init_filesystem_config(
            &git_dir,
            InitFilesystemConfigOptions {
                precompose_detection: PrecomposeDetection::Always,
                ..InitFilesystemConfigOptions::default()
            },
            &Environment::empty(),
        )
        .expect("apply");
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
        crate::repo::init_repository(
            root.path(),
            false,
            "main",
            Some(tmpl.path()),
            crate::RefStorageFormat::Files,
        )
        .expect("init");

        let text = fs::read_to_string(root.path().join(".git/config")).expect("read");
        assert!(
            text.contains("precomposeunicode = false"),
            "template local value must be preserved:\n{text}"
        );
    }

    #[test]
    fn symlink_detection_does_not_delete_template_file() {
        let td = TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        fs::write(git_dir.join("config"), "[core]\n").expect("config");
        let sentinel = git_dir.join(".grit-symlink-capability-00000");
        fs::write(&sentinel, "keep me").expect("sentinel");
        let _ = FilesystemCapabilities::detect_symlinks_supported(&git_dir);
        assert!(
            sentinel.is_file(),
            "detection must not remove an existing template file"
        );
        assert_eq!(fs::read_to_string(&sentinel).expect("read"), "keep me");
    }
}
