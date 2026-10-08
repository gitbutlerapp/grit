//! Explicit discovery and configuration environment for embedding callers.
//!
//! [`Environment`] holds the variables Git reads during repository discovery, config
//! cascade loading, identity, transport, and tracing. Library code must not read the
//! process environment for these values; embedders (including the `grit` CLI) construct
//! an [`Environment`] and pass it into
//! [`crate::repo::Repository::discover_with`](crate::repo::Repository::discover_with) and
//! [`ConfigSet::load`](crate::config::ConfigSet::load).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use time::{OffsetDateTime, UtcOffset};

use crate::git_date::tm::{local_tzoffset_with_tz, TzHhmm};
use crate::ident_resolve::IdentityEnv;

/// Pack/MIDX writer knobs captured from the environment at open time.
#[derive(Debug, Clone, Default)]
pub struct WriterEnvironment {
    /// When true, skip writing `.rev` reverse-index sidecars (from `GIT_TEST_NO_WRITE_REV_INDEX`).
    pub no_write_rev_index: bool,
    /// When set, controls MIDX embedded rev placeholder writes (`GIT_TEST_MIDX_WRITE_REV`).
    pub midx_write_rev: Option<bool>,
}

/// Reftable write behavior from the environment.
#[derive(Debug, Clone)]
pub struct ReftableEnvironment {
    /// When false, disable automatic reftable compaction (from `GIT_TEST_REFTABLE_AUTOCOMPACTION=false`).
    pub autocompaction: bool,
}

impl Default for ReftableEnvironment {
    fn default() -> Self {
        Self {
            autocompaction: true,
        }
    }
}

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
    pub grit_debug_safe_dir: bool,
    pub git_trace_setup: Option<String>,
    pub git_trace2_perf: Option<String>,
    pub git_trace: Option<String>,
    pub grit_invocation_cwd: Option<String>,
    pub sudo_uid: Option<String>,
    /// Windows `%ProgramFiles%` (for Git for Windows system config discovery).
    pub program_files: Option<String>,
    /// Windows `%ProgramFiles(x86)%`.
    pub program_files_x86: Option<String>,
    /// `USER` / `USERNAME` for identity fallbacks.
    pub user: Option<String>,
    pub username: Option<String>,
    pub git_author_name: Option<String>,
    pub git_author_email: Option<String>,
    pub git_author_date: Option<String>,
    pub git_committer_name: Option<String>,
    pub git_committer_email: Option<String>,
    pub git_committer_date: Option<String>,
    pub git_ssh: Option<OsString>,
    pub git_ssh_command: Option<OsString>,
    pub git_http_low_speed_limit: Option<String>,
    pub git_http_low_speed_time: Option<String>,
    pub git_notes_ref: Option<String>,
    pub git_notes_display_ref: Option<String>,
    pub git_attr_source: Option<String>,
    pub git_literal_pathspecs: Option<String>,
    pub git_glob_pathspecs: Option<String>,
    pub git_noglob_pathspecs: Option<String>,
    pub git_icase_pathspecs: Option<String>,
    pub git_index_version: Option<String>,
    pub git_print_sha1_ellipsis: Option<String>,
    /// Executable search path for helper lookup (e.g. GPG).
    pub path: Option<OsString>,
    /// Timezone for local date formatting (`TZ`).
    pub tz: Option<String>,
    /// Fixed "now" for tests (`GIT_TEST_DATE_NOW`), parsed at capture only.
    pub test_date_now: Option<i64>,
    /// When true, force split-index writes (`GIT_TEST_SPLIT_INDEX`).
    pub git_test_split_index: bool,
    /// Raw `GIT_TEST_COMMIT_GRAPH` (rev-list uses `"0"` to disable the commit-graph).
    pub git_test_commit_graph: Option<String>,
    /// Raw `GIT_TEST_CHECK_CACHE_TREE` (index write cache-tree verification).
    pub git_test_check_cache_tree: Option<String>,
    /// Networking debug trace (`GRIT_NET_DEBUG`).
    pub grit_net_debug: Option<String>,
    /// Terminal width hint (`COLUMNS`).
    pub columns: Option<String>,
    pub writer: WriterEnvironment,
    pub reftable: ReftableEnvironment,
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
            grit_debug_safe_dir: false,
            git_trace_setup: None,
            git_trace2_perf: None,
            git_trace: None,
            grit_invocation_cwd: None,
            sudo_uid: None,
            program_files: None,
            program_files_x86: None,
            user: None,
            username: None,
            git_author_name: None,
            git_author_email: None,
            git_author_date: None,
            git_committer_name: None,
            git_committer_email: None,
            git_committer_date: None,
            git_ssh: None,
            git_ssh_command: None,
            git_http_low_speed_limit: None,
            git_http_low_speed_time: None,
            git_notes_ref: None,
            git_notes_display_ref: None,
            git_attr_source: None,
            git_literal_pathspecs: None,
            git_glob_pathspecs: None,
            git_noglob_pathspecs: None,
            git_icase_pathspecs: None,
            git_index_version: None,
            git_print_sha1_ellipsis: None,
            path: None,
            tz: None,
            test_date_now: None,
            git_test_split_index: false,
            git_test_commit_graph: None,
            git_test_check_cache_tree: None,
            grit_net_debug: None,
            columns: None,
            writer: WriterEnvironment::default(),
            reftable: ReftableEnvironment::default(),
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

        let truthy = |v: &str| {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "yes" | "on")
        };

        let writer = WriterEnvironment {
            no_write_rev_index: get("GIT_TEST_NO_WRITE_REV_INDEX")
                .is_some_and(|v| truthy(&v)),
            midx_write_rev: get("GIT_TEST_MIDX_WRITE_REV").map(|v| truthy(&v)),
        };

        let reftable = ReftableEnvironment {
            autocompaction: get("GIT_TEST_REFTABLE_AUTOCOMPACTION")
                .map(|v| v.trim().to_ascii_lowercase() != "false")
                .unwrap_or(true),
        };

        let test_date_now = get("GIT_TEST_DATE_NOW").and_then(|s| s.parse::<i64>().ok());

        let git_test_split_index = get("GIT_TEST_SPLIT_INDEX")
            .map(|v| {
                let t = v.trim();
                t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes")
            })
            .unwrap_or(false);

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
            grit_debug_safe_dir: map.contains_key("GRIT_DEBUG_SAFE_DIR"),
            git_trace_setup: get("GIT_TRACE_SETUP"),
            git_trace2_perf: get("GIT_TRACE2_PERF"),
            git_trace: get("GIT_TRACE"),
            grit_invocation_cwd: get("GRIT_INVOCATION_CWD"),
            sudo_uid: get("SUDO_UID"),
            program_files: get("ProgramFiles"),
            program_files_x86: get("ProgramFiles(x86)"),
            user: get("USER"),
            username: get("USERNAME"),
            git_author_name: get("GIT_AUTHOR_NAME"),
            git_author_email: get("GIT_AUTHOR_EMAIL"),
            git_author_date: get("GIT_AUTHOR_DATE"),
            git_committer_name: get("GIT_COMMITTER_NAME"),
            git_committer_email: get("GIT_COMMITTER_EMAIL"),
            git_committer_date: get("GIT_COMMITTER_DATE"),
            git_ssh: get_os("GIT_SSH"),
            git_ssh_command: get_os("GIT_SSH_COMMAND"),
            git_http_low_speed_limit: get("GIT_HTTP_LOW_SPEED_LIMIT"),
            git_http_low_speed_time: get("GIT_HTTP_LOW_SPEED_TIME"),
            git_notes_ref: get("GIT_NOTES_REF"),
            git_notes_display_ref: get("GIT_NOTES_DISPLAY_REF"),
            git_attr_source: get("GIT_ATTR_SOURCE"),
            git_literal_pathspecs: get("GIT_LITERAL_PATHSPECS"),
            git_glob_pathspecs: get("GIT_GLOB_PATHSPECS"),
            git_noglob_pathspecs: get("GIT_NOGLOB_PATHSPECS"),
            git_icase_pathspecs: get("GIT_ICASE_PATHSPECS"),
            git_index_version: get("GIT_INDEX_VERSION"),
            git_print_sha1_ellipsis: get("GIT_PRINT_SHA1_ELLIPSIS"),
            path: get_os("PATH"),
            tz: get("TZ"),
            test_date_now,
            git_test_split_index,
            git_test_commit_graph: get("GIT_TEST_COMMIT_GRAPH"),
            git_test_check_cache_tree: get("GIT_TEST_CHECK_CACHE_TREE"),
            grit_net_debug: get("GRIT_NET_DEBUG"),
            columns: get("COLUMNS"),
            writer,
            reftable,
        }
    }

    /// Snapshot discovery/config variables from the current process (CLI and legacy call sites).
    #[must_use]
    pub fn capture_process() -> Self {
        Self::from_vars(std::env::vars_os(), process_cwd_fallback())
    }

    /// Current epoch seconds from this environment (fixed test time or system clock).
    #[must_use]
    pub fn now_epoch(&self) -> i64 {
        if let Some(epoch) = self.test_date_now {
            return epoch;
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Current instant as [`OffsetDateTime`] in the configured local timezone.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        offset_from_epoch_and_tz(self.now_epoch(), self.tz.as_deref())
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
            "USER" => self.user.clone(),
            "USERNAME" => self.username.clone(),
            "GIT_AUTHOR_NAME" => self.git_author_name.clone(),
            "GIT_AUTHOR_EMAIL" => self.git_author_email.clone(),
            "GIT_AUTHOR_DATE" => self.git_author_date.clone(),
            "GIT_COMMITTER_NAME" => self.git_committer_name.clone(),
            "GIT_COMMITTER_EMAIL" => self.git_committer_email.clone(),
            "GIT_COMMITTER_DATE" => self.git_committer_date.clone(),
            "GIT_HTTP_LOW_SPEED_LIMIT" => self.git_http_low_speed_limit.clone(),
            "GIT_HTTP_LOW_SPEED_TIME" => self.git_http_low_speed_time.clone(),
            "GIT_NOTES_REF" => self.git_notes_ref.clone(),
            "GIT_NOTES_DISPLAY_REF" => self.git_notes_display_ref.clone(),
            "GIT_ATTR_SOURCE" => self.git_attr_source.clone(),
            "GIT_LITERAL_PATHSPECS" => self.git_literal_pathspecs.clone(),
            "GIT_GLOB_PATHSPECS" => self.git_glob_pathspecs.clone(),
            "GIT_NOGLOB_PATHSPECS" => self.git_noglob_pathspecs.clone(),
            "GIT_ICASE_PATHSPECS" => self.git_icase_pathspecs.clone(),
            "GIT_INDEX_VERSION" => self.git_index_version.clone(),
            "GIT_PRINT_SHA1_ELLIPSIS" => self.git_print_sha1_ellipsis.clone(),
            "TZ" => self.tz.clone(),
            "GIT_TRACE" => self.git_trace.clone(),
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
        if key == "PATH" {
            return self.path.clone();
        }
        if key == "GIT_SSH" {
            return self.git_ssh.clone();
        }
        if key == "GIT_SSH_COMMAND" {
            return self.git_ssh_command.clone();
        }
        self.var(key).map(OsString::from)
    }

    /// Resolve `cwd` for discovery: canonicalize when possible without `env::current_dir`.
    #[must_use]
    pub fn discovery_cwd(&self) -> PathBuf {
        self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone())
    }

    /// Whether `GIT_PRINT_SHA1_ELLIPSIS` requests abbreviated OIDs in diff output.
    #[must_use]
    pub fn print_sha1_ellipsis(&self) -> bool {
        self.git_print_sha1_ellipsis
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("yes"))
    }

    /// Whether `GRIT_NET_DEBUG` enables networking trace output.
    #[must_use]
    pub fn grit_net_debug_enabled(&self) -> bool {
        self.grit_net_debug
            .as_deref()
            .is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
    }

    /// Resolve `GIT_INDEX_FILE` relative to [`Self::cwd`], or return `default`.
    #[must_use]
    pub fn resolve_index_file_path(&self, default: &std::path::Path) -> PathBuf {
        if let Some(raw) = self.git_index_file.as_deref() {
            if !raw.is_empty() {
                let p = PathBuf::from(raw);
                return if p.is_absolute() {
                    p
                } else {
                    self.cwd.join(p)
                };
            }
        }
        default.to_path_buf()
    }

    /// Process invocation cwd for hooks (`GRIT_INVOCATION_CWD` or [`Self::cwd`]).
    #[must_use]
    pub fn invocation_cwd(&self) -> PathBuf {
        if let Some(raw) = self.grit_invocation_cwd.as_deref() {
            if !raw.is_empty() {
                return PathBuf::from(raw);
            }
        }
        self.cwd.clone()
    }

    /// Default XDG or `~/.config/git/ignore` path when `core.excludesfile` is unset.
    #[must_use]
    pub fn default_global_ignore_path(&self) -> Option<String> {
        if let Some(xdg) = self.xdg_config_home.as_deref() {
            if !xdg.is_empty() {
                return Some(format!("{xdg}/git/ignore"));
            }
        }
        self.home
            .as_ref()
            .and_then(|h| h.to_str())
            .map(|home| format!("{home}/.config/git/ignore"))
    }

    /// Default XDG or `~/.config/git/attributes` path when `core.attributesfile` is unset.
    #[must_use]
    pub fn default_global_attributes_path(&self) -> Option<PathBuf> {
        let home = self.home.as_ref()?.to_str()?;
        if let Some(xdg) = self.xdg_config_home.as_deref() {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("git/attributes"));
            }
        }
        Some(PathBuf::from(home).join(".config/git/attributes"))
    }

    /// Author/committer date env for `@{now}` (`GIT_COMMITTER_DATE` then `GIT_AUTHOR_DATE`).
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
}

impl IdentityEnv for Environment {
    fn var(&self, key: &str) -> Option<String> {
        Environment::var(self, key)
    }

    fn var_os(&self, key: &str) -> Option<OsString> {
        Environment::var_os(self, key)
    }
}

/// Options passed to repository open/discover with an explicit [`Environment`].
#[derive(Debug, Clone)]
pub struct RepositoryOptions {
    /// Discovery and configuration environment.
    pub environment: Environment,
    /// When true, treat repository ownership as a different user (`safe.directory` tests).
    pub test_assume_different_owner: bool,
    /// Force `core.precomposeunicode` probe on init (Linux CI).
    pub force_precompose_probe: bool,
    /// When `Some(false)`, skip cache-tree verification on index write.
    pub verify_cache_tree_on_index_write: Option<bool>,
    /// When true, disable commit-graph acceleration in rev-list.
    pub disable_commit_graph: bool,
}

impl Default for RepositoryOptions {
    fn default() -> Self {
        Self {
            environment: Environment::empty(),
            test_assume_different_owner: false,
            force_precompose_probe: false,
            verify_cache_tree_on_index_write: None,
            disable_commit_graph: false,
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
        let disable_commit_graph = environment
            .git_test_commit_graph
            .as_deref()
            == Some("0");
        let verify_cache_tree_on_index_write =
            environment.git_test_check_cache_tree.as_ref().map(|v| {
                let t = v.trim();
                !(t.is_empty()
                    || t == "0"
                    || t.eq_ignore_ascii_case("false")
                    || t.eq_ignore_ascii_case("no"))
            });
        Self {
            environment,
            disable_commit_graph,
            verify_cache_tree_on_index_write,
            ..Self::default()
        }
    }

    /// Whether cache-tree verification runs on index write (default: true).
    #[must_use]
    pub fn verify_cache_tree(&self) -> bool {
        self.verify_cache_tree_on_index_write.unwrap_or(true)
    }
}

fn process_cwd_fallback() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn offset_from_epoch_and_tz(epoch: i64, tz: Option<&str>) -> OffsetDateTime {
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

    #[test]
    fn identity_env_reads_author_fields() {
        let vars = [
            ("GIT_AUTHOR_NAME", "Ada"),
            ("GIT_AUTHOR_EMAIL", "ada@example.com"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = Environment::from_vars(vars, PathBuf::from("/tmp"));
        assert_eq!(IdentityEnv::var(&env, "GIT_AUTHOR_NAME").as_deref(), Some("Ada"));
    }
}
