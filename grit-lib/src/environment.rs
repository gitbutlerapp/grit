//! Explicit discovery and configuration environment for embedding callers.
//!
//! [`Environment`] holds the variables Git reads during repository discovery and config
//! cascade loading. Library code must not read the process environment for these values;
//! embedders (including the `grit` CLI) construct an [`Environment`] and pass it into
//! [`crate::repo::Repository::discover_with`](crate::repo::Repository::discover_with) and
//! [`ConfigSet::load`](crate::config::ConfigSet::load).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// Discovery and config variables that affect repository open/discover and `ConfigSet` loading.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Process working directory used for relative path resolution during discovery.
    pub cwd: PathBuf,
    pub git_dir: Option<String>,
    pub git_work_tree: Option<String>,
    pub git_ceiling_directories: Option<String>,
    pub git_index_file: Option<String>,
    pub git_namespace: Option<String>,
    pub git_replace_ref_base: Option<String>,
    pub git_no_replace_objects: bool,
    pub git_alternate_object_directories: Option<String>,
    pub git_object_directory: Option<String>,
    /// When set, overrides the repository-relative cwd prefix derived from `cwd`.
    pub git_prefix: Option<String>,
    pub git_config_nosystem: Option<String>,
    pub git_config_system: Option<String>,
    pub git_config_global: Option<String>,
    pub git_config: Option<String>,
    pub git_config_parameters: Option<String>,
    pub git_config_count: Option<String>,
    /// Pairs from `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` when `GIT_CONFIG_COUNT` is set.
    pub git_config_pairs: Vec<(String, String)>,
    pub home: Option<OsString>,
    pub xdg_config_home: Option<String>,
    pub userprofile: Option<OsString>,
    pub homedrive: Option<OsString>,
    pub homepath: Option<OsString>,
    pub git_install_root: Option<OsString>,
    pub git_exec_path: Option<OsString>,
    /// `$PWD` when set (symlink-preserving absolute path for include matching).
    pub pwd: Option<String>,
    pub git_test_assume_different_owner: Option<String>,
    pub grit_debug_safe_dir: bool,
    pub git_trace_setup: Option<String>,
    pub git_trace2_perf: Option<String>,
    pub grit_invocation_cwd: Option<String>,
    pub sudo_uid: Option<String>,
    pub git_test_utf8_nfd_to_nfc: Option<String>,
    pub git_test_no_write_rev_index: Option<String>,
}

impl Environment {
    /// No overrides; [`Self::cwd`] is `"."` (canonicalized by callers when needed).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            cwd: PathBuf::from("."),
            git_dir: None,
            git_work_tree: None,
            git_ceiling_directories: None,
            git_index_file: None,
            git_namespace: None,
            git_replace_ref_base: None,
            git_no_replace_objects: false,
            git_alternate_object_directories: None,
            git_object_directory: None,
            git_prefix: None,
            git_config_nosystem: None,
            git_config_system: None,
            git_config_global: None,
            git_config: None,
            git_config_parameters: None,
            git_config_count: None,
            git_config_pairs: Vec::new(),
            home: None,
            xdg_config_home: None,
            userprofile: None,
            homedrive: None,
            homepath: None,
            git_install_root: None,
            git_exec_path: None,
            pwd: None,
            git_test_assume_different_owner: None,
            grit_debug_safe_dir: false,
            git_trace_setup: None,
            git_trace2_perf: None,
            grit_invocation_cwd: None,
            sudo_uid: None,
            git_test_utf8_nfd_to_nfc: None,
            git_test_no_write_rev_index: None,
        }
    }

    /// Parse known discovery/config variables from `vars` without reading the process environment.
    ///
    /// Unknown keys are ignored. `GIT_CONFIG_COUNT` expands into [`Self::git_config_pairs`].
    pub fn from_vars(vars: impl IntoIterator<Item = (OsString, OsString)>, cwd: PathBuf) -> Self {
        let map: HashMap<String, OsString> = vars
            .into_iter()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v))
            .collect();
        let get = |key: &str| -> Option<String> {
            map.get(key).and_then(|v| v.to_str().map(str::to_owned))
        };
        let get_os = |key: &str| -> Option<OsString> { map.get(key).cloned() };

        let mut git_config_pairs = Vec::new();
        if let Some(count_str) = get("GIT_CONFIG_COUNT") {
            if let Ok(count) = count_str.parse::<usize>() {
                for i in 0..count {
                    let key_var = format!("GIT_CONFIG_KEY_{i}");
                    let value_var = format!("GIT_CONFIG_VALUE_{i}");
                    if let (Some(k), Some(v)) = (get(&key_var), get(&value_var)) {
                        git_config_pairs.push((k, v));
                    }
                }
            }
        }

        Self {
            cwd,
            git_dir: get("GIT_DIR"),
            git_work_tree: get("GIT_WORK_TREE"),
            git_ceiling_directories: get("GIT_CEILING_DIRECTORIES"),
            git_index_file: get("GIT_INDEX_FILE"),
            git_namespace: get("GIT_NAMESPACE"),
            git_replace_ref_base: get("GIT_REPLACE_REF_BASE"),
            git_no_replace_objects: map.contains_key("GIT_NO_REPLACE_OBJECTS"),
            git_alternate_object_directories: get("GIT_ALTERNATE_OBJECT_DIRECTORIES"),
            git_object_directory: get("GIT_OBJECT_DIRECTORY"),
            git_prefix: get("GIT_PREFIX"),
            git_config_nosystem: get("GIT_CONFIG_NOSYSTEM"),
            git_config_system: get("GIT_CONFIG_SYSTEM"),
            git_config_global: get("GIT_CONFIG_GLOBAL"),
            git_config: get("GIT_CONFIG"),
            git_config_parameters: get("GIT_CONFIG_PARAMETERS"),
            git_config_count: get("GIT_CONFIG_COUNT"),
            git_config_pairs,
            home: get_os("HOME"),
            xdg_config_home: get("XDG_CONFIG_HOME"),
            userprofile: get_os("USERPROFILE"),
            homedrive: get_os("HOMEDRIVE"),
            homepath: get_os("HOMEPATH"),
            git_install_root: get_os("GIT_INSTALL_ROOT"),
            git_exec_path: get_os("GIT_EXEC_PATH"),
            pwd: get("PWD"),
            git_test_assume_different_owner: get("GIT_TEST_ASSUME_DIFFERENT_OWNER"),
            grit_debug_safe_dir: map.contains_key("GRIT_DEBUG_SAFE_DIR"),
            git_trace_setup: get("GIT_TRACE_SETUP"),
            git_trace2_perf: get("GIT_TRACE2_PERF"),
            grit_invocation_cwd: get("GRIT_INVOCATION_CWD"),
            sudo_uid: get("SUDO_UID"),
            git_test_utf8_nfd_to_nfc: get("GIT_TEST_UTF8_NFD_TO_NFC"),
            git_test_no_write_rev_index: get("GIT_TEST_NO_WRITE_REV_INDEX"),
        }
    }

    /// Whether `GIT_TEST_ASSUME_DIFFERENT_OWNER` is enabled.
    #[must_use]
    pub fn test_assume_different_owner(&self) -> bool {
        self.git_test_assume_different_owner
            .as_deref()
            .map(|v| {
                let lower = v.to_ascii_lowercase();
                v == "1" || lower == "true" || lower == "yes" || lower == "on"
            })
            .unwrap_or(false)
    }

    /// Snapshot discovery/config variables from the current process (CLI and legacy call sites).
    #[must_use]
    pub fn capture_process() -> Self {
        Self::from_vars(std::env::vars_os(), process_cwd_fallback())
    }

    /// Fingerprint of config-related fields for cascade cache keys (mirrors prior env-var scan).
    #[must_use]
    pub fn config_fingerprint(&self) -> Option<Vec<(String, Option<String>)>> {
        const VARS: [&str; 8] = [
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
            "XDG_CONFIG_HOME",
            "HOME",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ];
        let mut fp: Vec<(String, Option<String>)> = VARS
            .iter()
            .map(|name| {
                let val = match *name {
                    "GIT_CONFIG_NOSYSTEM" => self.git_config_nosystem.clone(),
                    "GIT_CONFIG_SYSTEM" => self.git_config_system.clone(),
                    "GIT_CONFIG_GLOBAL" => self.git_config_global.clone(),
                    "XDG_CONFIG_HOME" => self.xdg_config_home.clone(),
                    "HOME" => self
                        .home
                        .as_ref()
                        .and_then(|h| h.to_str().map(str::to_owned)),
                    "GIT_CONFIG" => self.git_config.clone(),
                    "GIT_CONFIG_PARAMETERS" => self.git_config_parameters.clone(),
                    "GIT_CONFIG_COUNT" => self.git_config_count.clone(),
                    _ => None,
                };
                ((*name).to_owned(), val)
            })
            .collect();
        if let Some(count_str) = &self.git_config_count {
            const MAX_TRACKED: usize = 256;
            match count_str.parse::<usize>() {
                Ok(n) if n <= MAX_TRACKED => {
                    for i in 0..n {
                        fp.push((
                            format!("GIT_CONFIG_KEY_{i}"),
                            self.git_config_pairs.get(i).map(|p| p.0.clone()),
                        ));
                        fp.push((
                            format!("GIT_CONFIG_VALUE_{i}"),
                            self.git_config_pairs.get(i).map(|p| p.1.clone()),
                        ));
                    }
                }
                Ok(_) => return None,
                Err(_) => {}
            }
        }
        Some(fp)
    }

    /// Return a string env var from this environment, if present and UTF-8.
    #[must_use]
    pub fn var(&self, key: &str) -> Option<String> {
        match key {
            "GIT_DIR" => self.git_dir.clone(),
            "GIT_WORK_TREE" => self.git_work_tree.clone(),
            "GIT_CEILING_DIRECTORIES" => self.git_ceiling_directories.clone(),
            "GIT_INDEX_FILE" => self.git_index_file.clone(),
            "GIT_NAMESPACE" => self.git_namespace.clone(),
            "GIT_REPLACE_REF_BASE" => self.git_replace_ref_base.clone(),
            "GIT_CONFIG_NOSYSTEM" => self.git_config_nosystem.clone(),
            "GIT_CONFIG_SYSTEM" => self.git_config_system.clone(),
            "GIT_CONFIG_GLOBAL" => self.git_config_global.clone(),
            "GIT_CONFIG" => self.git_config.clone(),
            "GIT_CONFIG_PARAMETERS" => self.git_config_parameters.clone(),
            "GIT_CONFIG_COUNT" => self.git_config_count.clone(),
            "GIT_PREFIX" => self.git_prefix.clone(),
            "GIT_TEST_UTF8_NFD_TO_NFC" => self.git_test_utf8_nfd_to_nfc.clone(),
            "XDG_CONFIG_HOME" => self.xdg_config_home.clone(),
            "HOME" => self
                .home
                .as_ref()
                .and_then(|h| h.to_str().map(str::to_owned)),
            "USERPROFILE" => self
                .userprofile
                .as_ref()
                .and_then(|h| h.to_str().map(str::to_owned)),
            "HOMEDRIVE" => self
                .homedrive
                .as_ref()
                .and_then(|h| h.to_str().map(str::to_owned)),
            "HOMEPATH" => self
                .homepath
                .as_ref()
                .and_then(|h| h.to_str().map(str::to_owned)),
            key if key.starts_with("GIT_CONFIG_KEY_") => {
                let idx: usize = key["GIT_CONFIG_KEY_".len()..].parse().ok()?;
                self.git_config_pairs.get(idx).map(|p| p.0.clone())
            }
            key if key.starts_with("GIT_CONFIG_VALUE_") => {
                let idx: usize = key["GIT_CONFIG_VALUE_".len()..].parse().ok()?;
                self.git_config_pairs.get(idx).map(|p| p.1.clone())
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn var_os(&self, key: &str) -> Option<OsString> {
        if key == "HOME" {
            return self.home.clone();
        }
        if key == "GIT_NO_REPLACE_OBJECTS" && self.git_no_replace_objects {
            return Some(OsString::new());
        }
        if key == "GIT_INSTALL_ROOT" {
            return self.git_install_root.clone();
        }
        if key == "GIT_EXEC_PATH" {
            return self.git_exec_path.clone();
        }
        self.var(key).map(OsString::from)
    }

    /// Resolve `cwd` for discovery: canonicalize when possible without `env::current_dir`.
    #[must_use]
    pub fn discovery_cwd(&self) -> PathBuf {
        self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone())
    }
}

/// Options passed to repository open/discover with an explicit [`Environment`].
#[derive(Debug, Clone)]
pub struct RepositoryOptions {
    /// Discovery and configuration environment.
    pub environment: Environment,
}

impl Default for RepositoryOptions {
    fn default() -> Self {
        Self {
            environment: Environment::empty(),
        }
    }
}

impl RepositoryOptions {
    /// Shorthand for [`Environment::empty`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_environment(environment: Environment) -> Self {
        Self { environment }
    }
}

fn process_cwd_fallback() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn from_vars_parses_config_count_pairs() {
        let vars = [
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "user.name"),
            ("GIT_CONFIG_VALUE_0", "Ada"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = Environment::from_vars(vars, PathBuf::from("/tmp"));
        assert_eq!(env.git_config_count.as_deref(), Some("1"));
        assert_eq!(
            env.git_config_pairs,
            vec![("user.name".to_owned(), "Ada".to_owned())]
        );
    }

    #[test]
    fn config_fingerprint_tracks_global_override() {
        let mut env = Environment::empty();
        env.git_config_global = Some("/tmp/global.cfg".to_owned());
        let fp = env.config_fingerprint().expect("fingerprint");
        assert!(fp
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_GLOBAL" && v.as_deref() == Some("/tmp/global.cfg")));
    }
}
