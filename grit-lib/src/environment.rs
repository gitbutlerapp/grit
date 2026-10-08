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
use std::sync::Arc;

use time::{OffsetDateTime, UtcOffset};

use crate::command_runner::{system_command_runner, CommandRunner};
use crate::diagnostics::{DiagnosticsHandle, NullDiagnostics};
use crate::git_date::tm::{local_tzoffset_with_tz, TzHhmm};
use crate::ident_resolve::IdentityEnv;

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
    pub git_author_name: Option<String>,
    pub git_author_email: Option<String>,
    pub git_author_date: Option<String>,
    pub git_committer_name: Option<String>,
    pub git_committer_email: Option<String>,
    pub git_committer_date: Option<String>,
    /// Timezone for local date formatting (`TZ`).
    pub tz: Option<String>,
    pub user: Option<String>,
    pub username: Option<String>,
    /// Windows `%ProgramFiles%` (for Git for Windows system config discovery).
    pub program_files: Option<String>,
    /// Windows `%ProgramFiles(x86)%`.
    pub program_files_x86: Option<String>,
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
            git_author_name: None,
            git_author_email: None,
            git_author_date: None,
            git_committer_name: None,
            git_committer_email: None,
            git_committer_date: None,
            tz: None,
            user: None,
            username: None,
            program_files: None,
            program_files_x86: None,
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
            git_author_name: get("GIT_AUTHOR_NAME"),
            git_author_email: get("GIT_AUTHOR_EMAIL"),
            git_author_date: get("GIT_AUTHOR_DATE"),
            git_committer_name: get("GIT_COMMITTER_NAME"),
            git_committer_email: get("GIT_COMMITTER_EMAIL"),
            git_committer_date: get("GIT_COMMITTER_DATE"),
            tz: get("TZ"),
            user: get("USER"),
            username: get("USERNAME"),
            program_files: get("ProgramFiles"),
            program_files_x86: get("ProgramFiles(x86)"),
        }
    }

    /// Parsed epoch from `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE` when set.
    #[must_use]
    pub fn git_now_date_override(&self) -> Option<i64> {
        let raw = self
            .git_committer_date
            .as_deref()
            .or(self.git_author_date.as_deref())?;
        raw.split_whitespace()
            .next()
            .and_then(|p| p.parse::<i64>().ok())
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
        fp.push((
            "GRIT_ENV_CWD".to_owned(),
            self.cwd.to_str().map(str::to_owned),
        ));
        fp.push(("GRIT_ENV_PWD".to_owned(), self.pwd.clone()));
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
            "GIT_AUTHOR_NAME" => self.git_author_name.clone(),
            "GIT_AUTHOR_EMAIL" => self.git_author_email.clone(),
            "GIT_AUTHOR_DATE" => self.git_author_date.clone(),
            "GIT_COMMITTER_NAME" => self.git_committer_name.clone(),
            "GIT_COMMITTER_EMAIL" => self.git_committer_email.clone(),
            "GIT_COMMITTER_DATE" => self.git_committer_date.clone(),
            "TZ" => self.tz.clone(),
            "USER" => self.user.clone(),
            "USERNAME" => self.username.clone(),
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

    /// Variables exported to hook/filter subprocesses (no process inheritance).
    #[must_use]
    pub fn subprocess_environment(&self) -> Vec<(OsString, OsString)> {
        let mut set = Vec::new();
        subprocess_push_str(&mut set, "GIT_DIR", self.git_dir.as_deref());
        subprocess_push_str(&mut set, "GIT_WORK_TREE", self.git_work_tree.as_deref());
        subprocess_push_str(
            &mut set,
            "GIT_CEILING_DIRECTORIES",
            self.git_ceiling_directories.as_deref(),
        );
        subprocess_push_str(&mut set, "GIT_INDEX_FILE", self.git_index_file.as_deref());
        subprocess_push_str(&mut set, "GIT_NAMESPACE", self.git_namespace.as_deref());
        subprocess_push_str(
            &mut set,
            "GIT_REPLACE_REF_BASE",
            self.git_replace_ref_base.as_deref(),
        );
        subprocess_push_str(
            &mut set,
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            self.git_alternate_object_directories.as_deref(),
        );
        subprocess_push_str(
            &mut set,
            "GIT_OBJECT_DIRECTORY",
            self.git_object_directory.as_deref(),
        );
        subprocess_push_str(&mut set, "GIT_PREFIX", self.git_prefix.as_deref());
        subprocess_push_str(
            &mut set,
            "GIT_CONFIG_NOSYSTEM",
            self.git_config_nosystem.as_deref(),
        );
        subprocess_push_str(
            &mut set,
            "GIT_CONFIG_SYSTEM",
            self.git_config_system.as_deref(),
        );
        subprocess_push_str(
            &mut set,
            "GIT_CONFIG_GLOBAL",
            self.git_config_global.as_deref(),
        );
        subprocess_push_str(&mut set, "GIT_CONFIG", self.git_config.as_deref());
        subprocess_push_str(
            &mut set,
            "GIT_CONFIG_PARAMETERS",
            self.git_config_parameters.as_deref(),
        );
        subprocess_push_str(
            &mut set,
            "GIT_CONFIG_COUNT",
            self.git_config_count.as_deref(),
        );
        for (i, (k, v)) in self.git_config_pairs.iter().enumerate() {
            set.push((
                OsString::from(format!("GIT_CONFIG_KEY_{i}")),
                OsString::from(k),
            ));
            set.push((
                OsString::from(format!("GIT_CONFIG_VALUE_{i}")),
                OsString::from(v),
            ));
        }
        subprocess_push_os(&mut set, "HOME", self.home.as_ref());
        subprocess_push_str(&mut set, "XDG_CONFIG_HOME", self.xdg_config_home.as_deref());
        subprocess_push_os(&mut set, "USERPROFILE", self.userprofile.as_ref());
        subprocess_push_os(&mut set, "HOMEDRIVE", self.homedrive.as_ref());
        subprocess_push_os(&mut set, "HOMEPATH", self.homepath.as_ref());
        subprocess_push_os(&mut set, "GIT_INSTALL_ROOT", self.git_install_root.as_ref());
        subprocess_push_os(&mut set, "GIT_EXEC_PATH", self.git_exec_path.as_ref());
        subprocess_push_str(&mut set, "PWD", self.pwd.as_deref());
        subprocess_push_str(
            &mut set,
            "GIT_TEST_ASSUME_DIFFERENT_OWNER",
            self.git_test_assume_different_owner.as_deref(),
        );
        subprocess_push_str(&mut set, "GIT_TRACE_SETUP", self.git_trace_setup.as_deref());
        subprocess_push_str(&mut set, "GIT_TRACE2_PERF", self.git_trace2_perf.as_deref());
        subprocess_push_str(
            &mut set,
            "GRIT_INVOCATION_CWD",
            self.grit_invocation_cwd.as_deref(),
        );
        subprocess_push_str(&mut set, "SUDO_UID", self.sudo_uid.as_deref());
        subprocess_push_str(
            &mut set,
            "GIT_TEST_UTF8_NFD_TO_NFC",
            self.git_test_utf8_nfd_to_nfc.as_deref(),
        );
        subprocess_push_str(
            &mut set,
            "GIT_TEST_NO_WRITE_REV_INDEX",
            self.git_test_no_write_rev_index.as_deref(),
        );
        subprocess_push_str(&mut set, "ProgramFiles", self.program_files.as_deref());
        subprocess_push_str(
            &mut set,
            "ProgramFiles(x86)",
            self.program_files_x86.as_deref(),
        );
        if self.git_no_replace_objects {
            set.push((OsString::from("GIT_NO_REPLACE_OBJECTS"), OsString::new()));
        }
        if self.grit_debug_safe_dir {
            set.push((OsString::from("GRIT_DEBUG_SAFE_DIR"), OsString::new()));
        }
        set
    }
}

impl IdentityEnv for Environment {
    fn var(&self, key: &str) -> Option<String> {
        Environment::var(self, key)
    }

    fn var_os(&self, key: &str) -> Option<OsString> {
        Environment::var_os(self, key)
    }
}

/// Map a Unix timestamp through an optional `TZ`-style string (or the process default offset).
#[must_use]
pub fn offset_from_epoch_and_tz(epoch: i64, tz: Option<&str>) -> OffsetDateTime {
    let tz_hhmm = local_tzoffset_with_tz(epoch as u64, tz);
    let offset = utc_offset_from_tz_hhmm(tz_hhmm).unwrap_or(UtcOffset::UTC);
    OffsetDateTime::from_unix_timestamp(epoch)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset)
}

fn utc_offset_from_tz_hhmm(tz: TzHhmm) -> Option<UtcOffset> {
    let sign: i32 = if tz < 0 { -1 } else { 1 };
    let abs = tz.unsigned_abs();
    let hours = (abs / 100) as i32;
    let minutes = (abs % 100) as i32;
    let seconds = sign * (hours * 3600 + minutes * 60);
    UtcOffset::from_whole_seconds(seconds).ok()
}

fn subprocess_push_str(out: &mut Vec<(OsString, OsString)>, key: &str, val: Option<&str>) {
    if let Some(v) = val {
        out.push((OsString::from(key), OsString::from(v)));
    }
}

fn subprocess_push_os(out: &mut Vec<(OsString, OsString)>, key: &str, val: Option<&OsString>) {
    if let Some(v) = val {
        out.push((OsString::from(key), v.clone()));
    }
}

/// Options passed to repository open/discover with an explicit [`Environment`].
#[derive(Clone)]
pub struct RepositoryOptions {
    /// Discovery and configuration environment.
    pub environment: Environment,
    /// Subprocess runner for hooks, filters, and helpers.
    pub command_runner: Arc<dyn CommandRunner>,
    /// Where non-fatal warnings and trace events are delivered.
    pub diagnostics: DiagnosticsHandle,
    /// When true, network operations may emit [`crate::diagnostics::Trace::Network`] events.
    pub network_trace: bool,
    /// When true, treat repository ownership as a different user (`safe.directory` tests).
    pub test_assume_different_owner: bool,
    /// When true, force split-index writes (embedder/test harness).
    pub force_split_index: bool,
    /// Wall-clock reference for rev-parse date selectors; embedders set explicitly.
    pub reference_unix_time: Option<i64>,
}

impl std::fmt::Debug for RepositoryOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepositoryOptions")
            .field("environment", &self.environment)
            .field("command_runner", &"<CommandRunner>")
            .field("network_trace", &self.network_trace)
            .finish_non_exhaustive()
    }
}

impl Default for RepositoryOptions {
    fn default() -> Self {
        Self {
            environment: Environment::empty(),
            command_runner: system_command_runner(),
            diagnostics: Arc::new(NullDiagnostics),
            network_trace: false,
            test_assume_different_owner: false,
            force_split_index: false,
            reference_unix_time: None,
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
        Self {
            environment,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_command_runner(mut self, runner: Arc<dyn CommandRunner>) -> Self {
        self.command_runner = runner;
        self
    }
}

fn process_cwd_fallback() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

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
