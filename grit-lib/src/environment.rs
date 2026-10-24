//! Explicit discovery and configuration environment for embedding callers.
//!
//! [`Environment`] holds the variables Git reads during repository discovery and config
//! cascade loading. Library code must not read the process environment for these values;
//! embedders (including the `grit` CLI) construct an [`Environment`] and pass it into
//! [`crate::repo::Repository::discover`](crate::repo::Repository::discover) and
//! [`crate::repo::Repository::open`](crate::repo::Repository::open), and
//! [`ConfigSet::load`](crate::config::ConfigSet::load).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use time::{OffsetDateTime, UtcOffset};

use crate::change_id::{ChangeIdGenerator, OsChangeIdGenerator};
use crate::command_runner::{system_command_runner, CommandRunner};
use crate::diagnostics::{DiagnosticsHandle, NullDiagnostics};
use crate::git_date::tm::{local_tzoffset_with_tz, TzHhmm};
use crate::ident_resolve::IdentityEnv;

/// Discovery and config variables that affect repository open/discover and `ConfigSet` loading.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Process working directory used for relative path resolution during discovery.
    pub cwd: PathBuf,
    /// Override for `GIT_DIR` when set (see Git environment variable of the same name).
    pub git_dir: Option<String>,
    /// Override for `GIT_WORK_TREE` when set (see Git environment variable of the same name).
    pub git_work_tree: Option<String>,
    /// Override for `GIT_CEILING_DIRECTORIES` when set (see Git environment variable of the same name).
    pub git_ceiling_directories: Option<String>,
    /// Override for `GIT_INDEX_FILE` when set (see Git environment variable of the same name).
    pub git_index_file: Option<String>,
    /// Override for `GIT_NAMESPACE` when set (see Git environment variable of the same name).
    pub git_namespace: Option<String>,
    /// Override for `GIT_REPLACE_REF_BASE` when set (see Git environment variable of the same name).
    pub git_replace_ref_base: Option<String>,
    /// When true, replacement objects from `GIT_REPLACE_OBJECT` are ignored.
    pub git_no_replace_objects: bool,
    /// Override for `GIT_ALTERNATE_OBJECT_DIRECTORIES` when set (see Git environment variable of the same name).
    pub git_alternate_object_directories: Option<String>,
    /// Override for `GIT_OBJECT_DIRECTORY` when set (see Git environment variable of the same name).
    pub git_object_directory: Option<String>,
    /// When set, overrides the repository-relative cwd prefix derived from `cwd`.
    pub git_prefix: Option<String>,
    /// Override for `GIT_CONFIG_NOSYSTEM` when set (see Git environment variable of the same name).
    pub git_config_nosystem: Option<String>,
    /// Override for `GIT_CONFIG_SYSTEM` when set (see Git environment variable of the same name).
    pub git_config_system: Option<String>,
    /// Override for `GIT_CONFIG_GLOBAL` when set (see Git environment variable of the same name).
    pub git_config_global: Option<String>,
    /// Override for `GIT_CONFIG` when set (see Git environment variable of the same name).
    pub git_config: Option<String>,
    /// Override for `GIT_CONFIG_PARAMETERS` when set (see Git environment variable of the same name).
    pub git_config_parameters: Option<String>,
    /// Override for `GIT_CONFIG_COUNT` when set (see Git environment variable of the same name).
    pub git_config_count: Option<String>,
    /// Pairs from `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` when `GIT_CONFIG_COUNT` is set.
    pub git_config_pairs: Vec<(String, String)>,
    /// Override for `HOME` when set (see Git environment variable of the same name).
    pub home: Option<OsString>,
    /// Override for `XDG_CONFIG_HOME` when set (see Git environment variable of the same name).
    pub xdg_config_home: Option<String>,
    /// Override for `USERPROFILE` when set (see Git environment variable of the same name).
    pub userprofile: Option<OsString>,
    /// Override for `HOMEDRIVE` when set (see Git environment variable of the same name).
    pub homedrive: Option<OsString>,
    /// Override for `HOMEPATH` when set (see Git environment variable of the same name).
    pub homepath: Option<OsString>,
    /// Override for `GIT_INSTALL_ROOT` when set (see Git environment variable of the same name).
    pub git_install_root: Option<OsString>,
    /// Override for `GIT_EXEC_PATH` when set (see Git environment variable of the same name).
    pub git_exec_path: Option<OsString>,
    /// `$PWD` when set (symlink-preserving absolute path for include matching).
    pub pwd: Option<String>,
    /// Override for `GIT_TEST_ASSUME_DIFFERENT_OWNER` when set (see Git environment variable of the same name).
    pub git_test_assume_different_owner: Option<String>,
    /// When true, relaxes safe-directory checks for debugging.
    pub grit_debug_safe_dir: bool,
    /// Override for `GIT_TRACE_SETUP` when set (see Git environment variable of the same name).
    pub git_trace_setup: Option<String>,
    /// Override for `GIT_TRACE2_PERF` when set (see Git environment variable of the same name).
    pub git_trace2_perf: Option<String>,
    /// Original working directory of the `grit` invocation (for messages).
    pub grit_invocation_cwd: Option<String>,
    /// Override for `SUDO_UID` when set (see Git environment variable of the same name).
    pub sudo_uid: Option<String>,
    /// Override for `GIT_TEST_UTF8_NFD_TO_NFC` when set (see Git environment variable of the same name).
    pub git_test_utf8_nfd_to_nfc: Option<String>,
    /// Override for `GIT_TEST_NO_WRITE_REV_INDEX` when set (see Git environment variable of the same name).
    pub git_test_no_write_rev_index: Option<String>,
    /// Test knob: when `"false"`, reftable auto-compaction is disabled (CLI captures at open).
    pub git_test_reftable_autocompaction: Option<String>,
    /// Override for `GIT_AUTHOR_NAME` when set (see Git environment variable of the same name).
    pub git_author_name: Option<String>,
    /// Override for `GIT_AUTHOR_EMAIL` when set (see Git environment variable of the same name).
    pub git_author_email: Option<String>,
    /// Override for `GIT_AUTHOR_DATE` when set (see Git environment variable of the same name).
    pub git_author_date: Option<String>,
    /// Override for `GIT_COMMITTER_NAME` when set (see Git environment variable of the same name).
    pub git_committer_name: Option<String>,
    /// Override for `GIT_COMMITTER_EMAIL` when set (see Git environment variable of the same name).
    pub git_committer_email: Option<String>,
    /// Override for `GIT_COMMITTER_DATE` when set (see Git environment variable of the same name).
    pub git_committer_date: Option<String>,
    /// Timezone for local date formatting (`TZ`).
    pub tz: Option<String>,
    /// Override for `USER` when set (see Git environment variable of the same name).
    pub user: Option<String>,
    /// Override for `USERNAME` when set (see Git environment variable of the same name).
    pub username: Option<String>,
    /// Windows `%ProgramFiles%` (for Git for Windows system config discovery).
    pub program_files: Option<String>,
    /// Windows `%ProgramFiles(x86)%`.
    pub program_files_x86: Option<String>,
    /// Full variable snapshot from [`Self::from_vars`] / [`Self::capture_process`].
    pub(crate) captured_vars: HashMap<String, OsString>,
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
            git_test_reftable_autocompaction: None,
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
            captured_vars: HashMap::new(),
        }
    }

    /// Whether this snapshot matches [`Self::empty`], the default
    /// [`crate::transfer::FetchOptions::environment`] value.
    ///
    /// SSH transport uses the repository [`Environment`] when fetch options still
    /// carry this sentinel so `FetchOptions::default()` does not mask `GIT_SSH_*`
    /// stored on the repository handle.
    #[must_use]
    pub fn is_fetch_options_default(&self) -> bool {
        let e = Self::empty();
        self.cwd == e.cwd
            && self.git_dir == e.git_dir
            && self.git_work_tree == e.git_work_tree
            && self.git_ceiling_directories == e.git_ceiling_directories
            && self.git_index_file == e.git_index_file
            && self.git_namespace == e.git_namespace
            && self.git_replace_ref_base == e.git_replace_ref_base
            && self.git_no_replace_objects == e.git_no_replace_objects
            && self.git_alternate_object_directories == e.git_alternate_object_directories
            && self.git_object_directory == e.git_object_directory
            && self.git_prefix == e.git_prefix
            && self.git_config_nosystem == e.git_config_nosystem
            && self.git_config_system == e.git_config_system
            && self.git_config_global == e.git_config_global
            && self.git_config == e.git_config
            && self.git_config_parameters == e.git_config_parameters
            && self.git_config_count == e.git_config_count
            && self.git_config_pairs == e.git_config_pairs
            && self.home == e.home
            && self.xdg_config_home == e.xdg_config_home
            && self.userprofile == e.userprofile
            && self.homedrive == e.homedrive
            && self.homepath == e.homepath
            && self.git_install_root == e.git_install_root
            && self.git_exec_path == e.git_exec_path
            && self.pwd == e.pwd
            && self.git_test_assume_different_owner == e.git_test_assume_different_owner
            && self.grit_debug_safe_dir == e.grit_debug_safe_dir
            && self.git_trace_setup == e.git_trace_setup
            && self.git_trace2_perf == e.git_trace2_perf
            && self.grit_invocation_cwd == e.grit_invocation_cwd
            && self.sudo_uid == e.sudo_uid
            && self.git_test_utf8_nfd_to_nfc == e.git_test_utf8_nfd_to_nfc
            && self.git_test_no_write_rev_index == e.git_test_no_write_rev_index
            && self.git_test_reftable_autocompaction == e.git_test_reftable_autocompaction
            && self.git_author_name == e.git_author_name
            && self.git_author_email == e.git_author_email
            && self.git_author_date == e.git_author_date
            && self.git_committer_name == e.git_committer_name
            && self.git_committer_email == e.git_committer_email
            && self.git_committer_date == e.git_committer_date
            && self.tz == e.tz
            && self.user == e.user
            && self.username == e.username
            && self.program_files == e.program_files
            && self.program_files_x86 == e.program_files_x86
            && self.captured_vars == e.captured_vars
    }

    /// Parse known discovery/config variables from `vars` without reading the process environment.
    ///
    /// Unknown keys from `vars` are retained for later lookup via [`Self::var`] / [`Self::var_os`].
    /// `GIT_CONFIG_COUNT` expands into [`Self::git_config_pairs`].
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
            git_test_reftable_autocompaction: get("GIT_TEST_REFTABLE_AUTOCOMPACTION"),
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
            captured_vars: map,
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

    /// Whether reftable auto-compaction is enabled for this process snapshot.
    ///
    /// Defaults to `true`. Set `GIT_TEST_REFTABLE_AUTOCOMPACTION=false` at the CLI
    /// boundary to disable (see site global-options docs).
    #[must_use]
    pub fn reftable_autocompaction_enabled(&self) -> bool {
        self.git_test_reftable_autocompaction
            .as_deref()
            .map(|value| value != "false")
            .unwrap_or(true)
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

    /// Use `path` as the repository directory, like setting `GIT_DIR`.
    ///
    /// A relative `path` is resolved against [`Self::cwd`] now, so the stored value stays
    /// correct when it is exported to subprocesses that run in another directory.
    ///
    /// # Parameters
    ///
    /// - `path`: the `.git` directory (or gitfile) to open instead of discovering one.
    ///
    /// # Returns
    ///
    /// This environment with [`Self::git_dir`] set.
    #[must_use]
    pub fn with_git_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.git_dir = Some(self.absolute_from_cwd(path.as_ref()));
        self
    }

    /// Use `path` as the work tree root, like setting `GIT_WORK_TREE`.
    ///
    /// A relative `path` is resolved against [`Self::cwd`] now, as in [`Self::with_git_dir`].
    ///
    /// # Parameters
    ///
    /// - `path`: the directory to treat as the top of the work tree.
    ///
    /// # Returns
    ///
    /// This environment with [`Self::git_work_tree`] set.
    #[must_use]
    pub fn with_work_tree(mut self, path: impl AsRef<Path>) -> Self {
        self.git_work_tree = Some(self.absolute_from_cwd(path.as_ref()));
        self
    }

    /// Append a command-line configuration override (`git -c key=value`).
    ///
    /// The override is added to [`Self::git_config_parameters`] in the quoted
    /// `'key'='value'` form (`'key'=` for a bare key) that Git itself writes to
    /// `GIT_CONFIG_PARAMETERS` for `git -c`, so it is read by
    /// [`ConfigSet::load`](crate::config::ConfigSet::load) with command-line scope and is
    /// inherited by subprocesses through [`Self::subprocess_environment`]. Later overrides win
    /// over earlier ones, over `GIT_CONFIG_COUNT` pairs, and over every config file.
    ///
    /// # Parameters
    ///
    /// - `config_override`: the validated key and optional value to add.
    pub fn push_config_override(&mut self, config_override: &ConfigOverride) {
        use crate::transport::sq_quote_shell_arg;

        let mut entry = sq_quote_shell_arg(&config_override.key);
        entry.push('=');
        if let Some(value) = &config_override.value {
            entry.push_str(&sq_quote_shell_arg(value));
        }
        match &mut self.git_config_parameters {
            Some(existing) if !existing.trim().is_empty() => {
                existing.push(' ');
                existing.push_str(&entry);
            }
            slot => *slot = Some(entry),
        }
    }

    fn absolute_from_cwd(&self, path: &Path) -> String {
        if path.is_absolute() {
            path.to_string_lossy().into_owned()
        } else {
            self.cwd.join(path).to_string_lossy().into_owned()
        }
    }

    /// Snapshot discovery/config variables from the current process (CLI and legacy call sites).
    #[must_use]
    pub fn capture_process() -> Self {
        // hygiene: CLI boundary — snapshot process environment into [`Environment`].
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
            "GIT_TEST_REFTABLE_AUTOCOMPACTION" => self.git_test_reftable_autocompaction.clone(),
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
            _ => self
                .captured_vars
                .get(key)
                .and_then(|v| v.to_str().map(str::to_owned)),
        }
    }

    #[must_use]
    /// Var os.
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
        if let Some(v) = self.var(key) {
            return Some(OsString::from(v));
        }
        self.captured_vars.get(key).cloned()
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

    /// User home directory using the same resolution as Git config loading.
    #[must_use]
    pub fn home_dir(&self) -> Option<PathBuf> {
        if let Some(home) = self.home.as_ref().filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home));
        }
        #[cfg(windows)]
        {
            if let (Some(drive), Some(path)) = (
                self.homedrive.as_ref().filter(|d| !d.is_empty()),
                self.homepath.as_ref().filter(|p| !p.is_empty()),
            ) {
                let mut combined = drive.clone();
                combined.push(path);
                return Some(PathBuf::from(combined));
            }
            if let Some(profile) = self.userprofile.as_ref().filter(|p| !p.is_empty()) {
                return Some(PathBuf::from(profile));
            }
        }
        None
    }

    /// Default path for `$XDG_CONFIG_HOME/git/{name}` or `$HOME/.config/git/{name}`.
    ///
    /// Used for Git's implicit global excludes (`ignore`) and attributes (`attributes`) files
    /// when `core.excludesfile` / `core.attributesfile` are unset.
    #[must_use]
    pub fn default_xdg_config_git_file(&self, name: &str) -> Option<PathBuf> {
        if let Some(xdg) = self.xdg_config_home.as_deref().filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(xdg).join("git").join(name));
        }
        self.home_dir()
            .map(|home| home.join(".config").join("git").join(name))
    }
}

/// One command-line configuration override, the `key[=value]` argument of `git -c`.
///
/// Parse one with [`str::parse`] (or [`ConfigOverride::new`]) and apply it with
/// [`Environment::push_config_override`]. The key is validated like any config key
/// (`section[.subsection].name`); a bare `key` with no `=` sets the boolean value `true`,
/// and `key=` sets the empty string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOverride {
    key: String,
    value: Option<String>,
}

impl ConfigOverride {
    /// Build an override from a key and optional value.
    ///
    /// # Parameters
    ///
    /// - `key`: config key such as `user.name` or `remote.origin.url`.
    /// - `value`: the value to set, or `None` for a boolean `true` (bare key).
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Config`] when `key` is not a valid config key.
    pub fn new(key: impl Into<String>, value: Option<String>) -> crate::error::Result<Self> {
        let key = key.into();
        crate::config::canonical_key(&key)?;
        Ok(Self { key, value })
    }

    /// The config key, as given.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The value, or `None` when the key was given without `=` (boolean `true`).
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

impl FromStr for ConfigOverride {
    type Err = crate::error::Error;

    /// Parse `key=value` (value may be empty or contain `=`) or a bare `key`.
    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        match spec.split_once('=') {
            Some((key, value)) => Self::new(key, Some(value.to_owned())),
            None => Self::new(spec, None),
        }
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

/// Nanoseconds since the Unix epoch from the process wall clock.
///
/// Callers pass this into [`crate::fsmonitor::FsmonitorRefreshOptions`] so fsmonitor hook
/// protocol v1 tokens are sampled before the hook subprocess runs.
#[must_use]
pub fn wall_clock_unix_timestamp_nanos() -> u64 {
    u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(u64::MAX)
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
#[non_exhaustive]
pub struct RepositoryOptions {
    /// Discovery and configuration environment.
    pub environment: Environment,
    /// Subprocess runner for hooks, filters, and helpers.
    pub command_runner: Arc<dyn CommandRunner>,
    /// Random bytes for new change ids (`grit.changeIds`).
    pub change_id_generator: Arc<dyn ChangeIdGenerator>,
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
    /// Parallel worker overrides (`--threads` in the CLI).
    pub thread_policy: crate::parallel::ThreadPolicy,
    /// Work tree path for [`crate::repo::Repository::open`] (non-bare repos).
    pub work_tree: Option<std::path::PathBuf>,
    /// When false, skip [`crate::repo::validate_repo_format`] during open.
    pub validate_repository_format: bool,
    /// When true, [`crate::repo::Repository::open`] resolves `path` like wire-protocol serving.
    pub open_for_serving: bool,
    /// Optional on-demand fetcher for partial-clone promisor objects.
    pub promisor_fetcher: Option<std::sync::Arc<dyn crate::promisor_fetch::PromisorFetcher>>,
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
            change_id_generator: Arc::new(OsChangeIdGenerator),
            diagnostics: Arc::new(NullDiagnostics),
            network_trace: false,
            test_assume_different_owner: false,
            force_split_index: false,
            reference_unix_time: None,
            thread_policy: crate::parallel::ThreadPolicy::default(),
            work_tree: None,
            validate_repository_format: true,
            open_for_serving: false,
            promisor_fetcher: None,
        }
    }
}

impl RepositoryOptions {
    /// Shorthand for [`Environment::empty`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build options around an explicit [`Environment`] snapshot.
    #[must_use]
    pub fn with_environment(environment: Environment) -> Self {
        Self {
            environment,
            ..Self::default()
        }
    }

    /// Override the injectable subprocess runner used for hooks and filters.
    #[must_use]
    pub fn with_command_runner(mut self, runner: Arc<dyn CommandRunner>) -> Self {
        self.command_runner = runner;
        self
    }

    /// Override the generator used for random change ids on new commits.
    #[must_use]
    pub fn with_change_id_generator(mut self, generator: Arc<dyn ChangeIdGenerator>) -> Self {
        self.change_id_generator = generator;
        self
    }

    /// Attach an explicit work tree for [`crate::repo::Repository::open`].
    #[must_use]
    pub fn with_work_tree(mut self, work_tree: impl Into<std::path::PathBuf>) -> Self {
        self.work_tree = Some(work_tree.into());
        self
    }

    /// Skip repository format validation when opening (discovery follow-up paths).
    #[must_use]
    pub fn without_format_validation(mut self) -> Self {
        self.validate_repository_format = false;
        self
    }

    /// Resolve `path` for smart-HTTP / upload-pack serving instead of a bare git directory.
    #[must_use]
    pub fn for_serving(mut self) -> Self {
        self.open_for_serving = true;
        self
    }

    /// Override the diagnostic sink for warnings and trace events.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: DiagnosticsHandle) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Enable or disable network trace events.
    #[must_use]
    pub fn with_network_trace(mut self, network_trace: bool) -> Self {
        self.network_trace = network_trace;
        self
    }

    /// Install a promisor on-demand object fetcher for repositories opened with these options.
    #[must_use]
    pub fn with_promisor_fetcher(
        mut self,
        fetcher: Option<std::sync::Arc<dyn crate::promisor_fetch::PromisorFetcher>>,
    ) -> Self {
        self.promisor_fetcher = fetcher;
        self
    }
}

fn process_cwd_fallback() -> PathBuf {
    // hygiene: used only from [`Environment::capture_process`].
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_fetch_options_default_matches_empty() {
        assert!(Environment::empty().is_fetch_options_default());
        let mut env = Environment::from_vars(
            [(
                OsString::from("GIT_SSH_COMMAND"),
                OsString::from("/bin/wrap"),
            )],
            PathBuf::from("."),
        );
        assert!(!env.is_fetch_options_default());
        env.captured_vars.clear();
        assert!(env.is_fetch_options_default());
    }

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

    #[test]
    fn default_xdg_config_git_file_prefers_xdg() {
        let mut env = Environment::empty();
        env.home = Some(OsString::from("/home/u"));
        env.xdg_config_home = Some("/xdg".to_owned());
        assert_eq!(
            env.default_xdg_config_git_file("ignore"),
            Some(PathBuf::from("/xdg/git/ignore"))
        );
    }

    fn isolated_env(dir: &Path) -> Environment {
        let mut env = Environment::empty();
        env.cwd = dir.to_path_buf();
        env.home = Some(dir.as_os_str().to_owned());
        env.git_config_nosystem = Some("1".to_owned());
        env.git_config_global = Some(dir.join("no-global").to_string_lossy().into_owned());
        env
    }

    fn override_of(spec: &str) -> ConfigOverride {
        spec.parse().expect("valid override")
    }

    #[test]
    fn config_override_parses_key_value_bare_and_empty() {
        let o = override_of("user.name=Ada Lovelace");
        assert_eq!((o.key(), o.value()), ("user.name", Some("Ada Lovelace")));
        let o = override_of("http.extraHeader=Authorization: a=b");
        assert_eq!(o.value(), Some("Authorization: a=b"));
        let o = override_of("core.bare");
        assert_eq!((o.key(), o.value()), ("core.bare", None));
        let o = override_of("user.email=");
        assert_eq!(o.value(), Some(""));
        let o = override_of("remote.My Origin.url=x");
        assert_eq!(o.key(), "remote.My Origin.url");
    }

    #[test]
    fn config_override_rejects_invalid_keys() {
        for spec in [
            "nosection",
            "=value",
            "user.=x",
            ".name=x",
            "us er.name=x",
            "",
        ] {
            assert!(
                spec.parse::<ConfigOverride>().is_err(),
                "{spec:?} should be rejected"
            );
        }
        assert!(ConfigOverride::new("a\nb.c", None).is_err());
    }

    #[test]
    fn push_config_override_round_trips_through_parameter_parser() {
        let mut env = Environment::empty();
        let tricky = "it's a \"quote\" = !bang  spaced";
        env.push_config_override(
            &ConfigOverride::new("user.name", Some(tricky.to_owned())).unwrap(),
        );
        env.push_config_override(&override_of("core.bare"));
        env.push_config_override(&override_of("user.email="));
        let raw = env.git_config_parameters.clone().expect("parameters set");
        assert_eq!(
            crate::config::parse_config_parameters(&raw),
            vec![
                format!("user.name\u{1}{tricky}"),
                "core.bare\u{1}".to_owned(),
                "user.email\u{1}".to_owned(),
            ]
        );
    }

    #[test]
    fn push_config_override_appends_to_inherited_parameters() {
        let mut env = Environment::empty();
        env.git_config_parameters = Some("'core.editor'='vi'".to_owned());
        env.push_config_override(&override_of("user.name=CI"));
        assert_eq!(
            env.git_config_parameters.as_deref(),
            Some("'core.editor'='vi' 'user.name'='CI'")
        );
        let mut blank = Environment::empty();
        blank.git_config_parameters = Some("  ".to_owned());
        blank.push_config_override(&override_of("user.name=CI"));
        assert_eq!(
            blank.git_config_parameters.as_deref(),
            Some("'user.name'='CI'")
        );
    }

    #[test]
    fn later_config_override_wins_over_earlier_env_pairs_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[user]\n\tname = FromRepo\n\temail = repo@x.y\n[core]\n\tbare = false\n",
        )
        .unwrap();
        let mut env = isolated_env(dir.path());
        env.git_config_count = Some("1".to_owned());
        env.git_config_pairs = vec![("user.name".to_owned(), "FromCount".to_owned())];
        env.push_config_override(&override_of("user.name=First"));
        env.push_config_override(&override_of("user.name=Second"));
        env.push_config_override(&override_of("core.bare"));

        let config = crate::config::ConfigSet::load(&env, Some(&git_dir), false).unwrap();
        assert_eq!(config.get("user.name").as_deref(), Some("Second"));
        assert_eq!(config.get("user.email").as_deref(), Some("repo@x.y"));
        assert_eq!(config.get_bool("core.bare"), Some(Ok(true)));
    }

    #[test]
    fn config_override_is_exported_to_subprocesses() {
        let mut env = Environment::empty();
        env.push_config_override(&override_of("user.name=CI"));
        assert!(env.subprocess_environment().contains(&(
            OsString::from("GIT_CONFIG_PARAMETERS"),
            OsString::from("'user.name'='CI'")
        )));
    }

    #[test]
    fn with_git_dir_and_work_tree_resolve_relative_paths_against_cwd() {
        let mut env = Environment::empty();
        env.cwd = PathBuf::from("/base/dir");
        let env = env.with_git_dir("repo/.git").with_work_tree("wt");
        assert_eq!(
            env.git_dir.as_deref().map(PathBuf::from),
            Some(PathBuf::from("/base/dir/repo/.git"))
        );
        assert_eq!(
            env.git_work_tree.as_deref().map(PathBuf::from),
            Some(PathBuf::from("/base/dir/wt"))
        );
        let abs = std::env::temp_dir().join("abs.git");
        let env = env.with_git_dir(&abs);
        assert_eq!(env.git_dir.as_deref().map(PathBuf::from), Some(abs));
    }

    #[test]
    fn with_git_dir_opens_that_repository_from_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("r");
        crate::repo::init_repository(
            &repo_dir,
            false,
            "main",
            None,
            crate::ref_storage::RefStorageFormat::Files,
        )
        .unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let mut env = isolated_env(&elsewhere);
        env.cwd = elsewhere.clone();
        let env = env.with_git_dir("../r/.git").with_work_tree("../r");
        let repo =
            crate::repo::Repository::discover(None, &RepositoryOptions::with_environment(env))
                .unwrap();
        assert_eq!(
            repo.git_dir().canonicalize().unwrap(),
            repo_dir.join(".git").canonicalize().unwrap()
        );
        assert_eq!(
            repo.work_tree().map(|p| p.canonicalize().unwrap()),
            Some(repo_dir.canonicalize().unwrap())
        );
    }

    #[test]
    fn default_xdg_config_git_file_falls_back_to_home_config() {
        let mut env = Environment::empty();
        env.home = Some(OsString::from("/home/u"));
        assert_eq!(
            env.default_xdg_config_git_file("ignore"),
            Some(PathBuf::from("/home/u/.config/git/ignore"))
        );
    }
}
