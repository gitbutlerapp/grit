//! Remote configuration and transport operations scoped to a [`Repository`](crate::repo::Repository).

use std::collections::{BTreeSet, HashSet};

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::error::Result;
use crate::fetch::Progress;
use crate::remote::{
    HttpClientFactory, ListRefsOptions, Remote, RemoteError, RemoteRef, DEFAULT_REMOTE,
};
use crate::repo::Repository;
use crate::transfer::{FetchOptions, FetchOutcome, PushOptions, PushOutcome, PushRefSpec};
use crate::url_rewrite;

/// Remote names and network operations for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryRemotes<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryRemotes<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Names of remotes with a configured `remote.<name>.url` in the merged config cascade.
    ///
    /// # Errors
    ///
    /// Propagates [`Repository::config`] failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let names = repo.remotes().list()?;
    /// let _ = names;
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self) -> Result<Vec<String>> {
        let config = self.repo.config()?;
        Ok(configured_remote_names(config.as_ref()))
    }

    /// Every remote with a `remote.<name>.*` section, sorted by name, with its URL.
    ///
    /// Unlike [`Self::list`], this includes remotes that have config but no
    /// `remote.<name>.url` (their [`ConfiguredRemote::url`] is `None`).
    ///
    /// # Errors
    ///
    /// Propagates [`Repository::config`] failures.
    pub fn configured(&self) -> Result<Vec<ConfiguredRemote>> {
        let config = self.repo.config()?;
        Ok(configured_remotes(config.as_ref()))
    }

    /// Replace the fetch or push URL for an existing remote.
    ///
    /// Fetch updates `remote.<name>.url`. Push removes every `remote.<name>.pushurl` entry and
    /// writes a single new one, matching `git remote set-url --push`. Writes the shared
    /// repository config (same rules as [`Self::add`]), then reloads config unless `dry_run`.
    ///
    /// # Errors
    ///
    /// - [`RemoteError::UnknownRemote`] when no `remote.<name>.*` section exists.
    /// - [`RemoteError::Library`] when config cannot be read or written.
    pub fn set_url(
        &self,
        name: &str,
        url: &str,
        kind: RemoteUrlKind,
        dry_run: bool,
    ) -> std::result::Result<UrlChange, RemoteError> {
        let config = self.repo.config().map_err(RemoteError::Library)?;
        if !has_remote_section(config.as_ref(), name) {
            return Err(RemoteError::UnknownRemote(name.to_owned()));
        }
        let old = match kind {
            RemoteUrlKind::Fetch => config.get(&format!("remote.{name}.url")),
            RemoteUrlKind::Push => config
                .get(&format!("remote.{name}.pushurl"))
                .filter(|u| !u.trim().is_empty()),
        };
        let change = UrlChange {
            old,
            new: url.to_owned(),
        };
        if dry_run {
            return Ok(change);
        }
        let path = crate::repo::common_git_dir_for_config(self.repo.git_dir()).join("config");
        let mut file = match ConfigFile::from_path(&path, ConfigScope::Local)? {
            Some(file) => file,
            None => ConfigFile::parse(&path, "", ConfigScope::Local)?,
        };
        match kind {
            RemoteUrlKind::Fetch => {
                file.set(&format!("remote.{name}.url"), url)?;
            }
            RemoteUrlKind::Push => {
                file.unset(&format!("remote.{name}.pushurl"))?;
                file.set(&format!("remote.{name}.pushurl"), url)?;
            }
        }
        file.write()?;
        self.repo.reload_config()?;
        Ok(change)
    }

    /// Add remote `name` at `url` to the repository's local config.
    ///
    /// Writes `remote.<name>.url` and the default fetch refspec
    /// `+refs/heads/*:refs/remotes/<name>/*` to the shared repository config (the common
    /// directory's `config`, also from a linked worktree that has its own admin `config`),
    /// then reloads [`Repository::config`]. The URL is stored as given and
    /// not validated, like Git.
    ///
    /// # Errors
    ///
    /// - [`RemoteError::InvalidRemoteName`] when `refs/remotes/<name>/…` would not be a valid ref.
    /// - [`RemoteError::RemoteExists`] when any `remote.<name>.*` key is already configured.
    /// - [`RemoteError::Library`] when the config cannot be read, parsed, or written.
    pub fn add(&self, name: &str, url: &str) -> std::result::Result<(), RemoteError> {
        if !is_valid_remote_name(name) {
            return Err(RemoteError::InvalidRemoteName(name.to_owned()));
        }
        if has_remote_section(self.repo.config()?.as_ref(), name) {
            return Err(RemoteError::RemoteExists(name.to_owned()));
        }
        // Git keeps remotes in the common config even when a worktree admin dir has its own.
        let path = crate::repo::common_git_dir_for_config(self.repo.git_dir()).join("config");
        let mut file = match ConfigFile::from_path(&path, ConfigScope::Local)? {
            Some(file) => file,
            None => ConfigFile::parse(&path, "", ConfigScope::Local)?,
        };
        file.set(&format!("remote.{name}.url"), url)?;
        file.set(
            &format!("remote.{name}.fetch"),
            &format!("+refs/heads/*:refs/remotes/{name}/*"),
        )?;
        file.write()?;
        self.repo.reload_config()?;
        Ok(())
    }

    /// Load `remote.<name>` URL(s) and fetch refspecs from config.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError::NoUrl`] when the remote is missing or has no URL.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// if let Ok(origin) = repo.remotes().get("origin") {
    ///     let _url = origin.fetch_url();
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn get(&self, name: &str) -> std::result::Result<Remote, RemoteError> {
        let config = self.repo.config().map_err(RemoteError::Library)?;
        Remote::from_config(config.as_ref(), name)
    }

    /// Resolve a remote by name, falling back to [`DEFAULT_REMOTE`] when `name` is empty.
    ///
    /// # Errors
    ///
    /// Same as [`Self::get`].
    pub fn get_or_default(&self, name: &str) -> std::result::Result<Remote, RemoteError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            self.get(DEFAULT_REMOTE)
        } else {
            self.get(trimmed)
        }
    }

    /// List references on `remote` ([`Remote::list_refs`] with this repository as context).
    ///
    /// # Errors
    ///
    /// Propagates [`RemoteError`] from the wire or local transport.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::remote::{ListRefsOptions, Remote};
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let remote = Remote::from_url("/path/to/other.git")?;
    /// let refs = repo.remotes().ls_remote(&remote, &ListRefsOptions::default(), None)?;
    /// let _ = refs.len();
    /// # Ok(())
    /// # }
    /// ```
    pub fn ls_remote(
        &self,
        remote: &Remote,
        options: &ListRefsOptions,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> std::result::Result<Vec<RemoteRef>, RemoteError> {
        remote.list_refs(Some(self.repo), options, http_factory)
    }

    /// Fetch from `remote` into this repository.
    ///
    /// # Errors
    ///
    /// Propagates transport and ref-update failures as [`RemoteError`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::remote::fetch::NoProgress;
    /// # use grit_lib::remote::Remote;
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::remote::transfer::FetchOptions;
    /// # use std::path::Path;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let remote = Remote::from_url("/path/to/upstream.git")?;
    /// let mut progress = NoProgress;
    /// let _out = repo
    ///     .remotes()
    ///     .fetch(&remote, FetchOptions::default(), &mut progress, None)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn fetch(
        &self,
        remote: &Remote,
        options: FetchOptions,
        progress: &mut dyn Progress,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> std::result::Result<FetchOutcome, RemoteError> {
        remote.fetch(self.repo, options, progress, http_factory)
    }

    /// Fetch from every configured remote in config load order, skipping
    /// `remote.<name>.skipFetchAll=true` entries.
    ///
    /// Each remote uses [`resolve_fetch_prune`](crate::remote::transfer::resolve_fetch_prune)
    /// and [`resolve_fetch_tags`](crate::remote::transfer::resolve_fetch_tags) with
    /// `cli_prune` / `cli_tags` when set; otherwise that remote's own config.
    /// Failures are recorded per remote and fetching continues.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Config`] when prune config is invalid for
    /// a remote (same as
    /// [`resolve_fetch_prune`](crate::remote::transfer::resolve_fetch_prune)).
    pub fn fetch_all(
        &self,
        config: &ConfigSet,
        base: &FetchOptions,
        cli_prune: Option<bool>,
        cli_tags: Option<bool>,
        progress: &mut dyn Progress,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> Result<Vec<(String, std::result::Result<FetchOutcome, RemoteError>)>> {
        use crate::remote::transfer::{resolve_fetch_prune, resolve_fetch_tags};

        let mut results = Vec::new();
        for name in configured_remote_names_in_config_order(config) {
            if remote_skip_fetch_all(config, &name) {
                continue;
            }
            let prune = resolve_fetch_prune(config, &name, cli_prune)?;
            let tags = resolve_fetch_tags(config, &name, cli_tags);
            let opts = FetchOptions {
                tags,
                prune,
                dry_run: base.dry_run,
                fetch_depth: base.fetch_depth,
                deepen_since: base.deepen_since.clone(),
                deepen_not: base.deepen_not.clone(),
                index_pack_threads: base.index_pack_threads,
                thread_policy: self.repo.thread_policy(),
                environment: base.environment.clone(),
                pack_spool_only: base.pack_spool_only,
                http_protocol_version: base.http_protocol_version,
                remote_progress: base.remote_progress,
                ..Default::default()
            };
            let outcome = self
                .get(&name)
                .and_then(|remote| self.fetch(&remote, opts, progress, http_factory));
            results.push((name, outcome));
        }
        Ok(results)
    }

    /// Push `refs` to `remote` from this repository.
    ///
    /// # Errors
    ///
    /// Propagates transport and ref-update failures as [`RemoteError`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::remote::fetch::NoProgress;
    /// # use grit_lib::remote::Remote;
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
    /// # use std::path::Path;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let remote = Remote::from_url("/path/to/upstream.git")?;
    /// let mut progress = NoProgress;
    /// let refs: Vec<PushRefSpec> = Vec::new();
    /// let _out = repo
    ///     .remotes()
    ///     .push(&remote, &refs, PushOptions::default(), &mut progress, None)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn push(
        &self,
        remote: &Remote,
        refs: &[PushRefSpec],
        options: PushOptions,
        progress: &mut dyn Progress,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> std::result::Result<PushOutcome, RemoteError> {
        remote.push(self.repo, refs, options, progress, http_factory)
    }
}

/// Whether `remote.<name>.skipFetchAll` disables [`RepositoryRemotes::fetch_all`].
#[must_use]
pub fn remote_skip_fetch_all(config: &ConfigSet, remote: &str) -> bool {
    match config.get_bool(&format!("remote.{remote}.skipFetchAll")) {
        None | Some(Ok(false)) => false,
        Some(Ok(true)) => true,
        Some(Err(_)) => false,
    }
}

/// Remote names in merged config load order (first `remote.<name>.url` wins ordering).
#[must_use]
pub fn configured_remote_names_in_config_order(config: &ConfigSet) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some((name, key)) = rest.rsplit_once('.') else {
            continue;
        };
        if key == "url" && !name.is_empty() && seen.insert(name.to_owned()) {
            names.push(name.to_owned());
        }
    }
    names
}

/// Which URL field [`RepositoryRemotes::set_url`] updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteUrlKind {
    /// `remote.<name>.url` (fetch).
    Fetch,
    /// `remote.<name>.pushurl` (replaces all push URLs with one).
    Push,
}

/// Result of [`RepositoryRemotes::set_url`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlChange {
    /// Previous URL for the selected kind, if any.
    pub old: Option<String>,
    /// New URL written (or that would be written when dry-run).
    pub new: String,
}

/// A remote that has a `remote.<name>.*` section in config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredRemote {
    /// Remote name (the config subsection).
    pub name: String,
    /// Last `remote.<name>.url` value, or `None` when the section has no URL.
    pub url: Option<String>,
    /// Raw `remote.<name>.pushurl` values from config (may be empty).
    pub push_urls: Vec<String>,
    /// Raw `remote.<name>.fetch` refspecs (empty when unset).
    pub fetch_refspecs: Vec<String>,
    /// Fetch URL after `url.*.insteadOf`, when [`Self::url`] is set.
    pub effective_fetch_url: Option<String>,
    /// Push URL after `pushInsteadOf` / `insteadOf`; equals [`Self::effective_fetch_url`] when no push URL is configured.
    pub effective_push_url: Option<String>,
}

fn configured_remote_from_config(config: &ConfigSet, name: String) -> ConfiguredRemote {
    let url = config.get(&format!("remote.{name}.url"));
    let mut fetch_refspecs = config.get_all(&format!("remote.{name}.fetch"));
    fetch_refspecs.retain(|s| !s.trim().is_empty());
    let push_urls: Vec<String> = config
        .get_all(&format!("remote.{name}.pushurl"))
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect();
    let (effective_fetch_url, effective_push_url) = match url.as_deref().filter(|u| !u.is_empty()) {
        Some(raw) => {
            let effective_fetch_url = url_rewrite::rewrite_fetch_url(config, raw);
            let push_source = config
                .get(&format!("remote.{name}.pushurl"))
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| raw.to_owned());
            let effective_push_url = url_rewrite::rewrite_push_url(config, push_source.as_str());
            (Some(effective_fetch_url), Some(effective_push_url))
        }
        None => (None, None),
    };
    ConfiguredRemote {
        name,
        url,
        push_urls,
        fetch_refspecs,
        effective_fetch_url,
        effective_push_url,
    }
}

/// Every remote with at least one `remote.<name>.*` key, sorted by name.
///
/// This is the set Git treats as configured remotes (for listing and for refusing to
/// add a duplicate), which is wider than [`configured_remote_names`].
#[must_use]
pub fn configured_remotes(config: &ConfigSet) -> Vec<ConfiguredRemote> {
    let mut names = BTreeSet::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        if let Some((name, _key)) = rest.rsplit_once('.') {
            names.insert(name);
        }
    }
    names
        .into_iter()
        .map(|name| configured_remote_from_config(config, name.to_owned()))
        .collect()
}

/// Whether any `remote.<name>.*` key exists (membership in [`configured_remotes`]).
pub(crate) fn has_remote_section(config: &ConfigSet, name: &str) -> bool {
    config.entries().iter().any(|entry| {
        entry
            .key
            .strip_prefix("remote.")
            .and_then(|rest| rest.rsplit_once('.'))
            .is_some_and(|(section, _key)| section == name)
    })
}

/// Whether `name` can name a remote: its tracking refs `refs/remotes/<name>/…` must be valid.
fn is_valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && check_refname_format(
            &format!("refs/remotes/{name}/test"),
            &RefNameOptions::default(),
        )
        .is_ok()
}

/// Collect remote subsection names that declare a `url` key.
#[must_use]
pub fn configured_remote_names(config: &ConfigSet) -> Vec<String> {
    let mut names = BTreeSet::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some((name, key)) = rest.rsplit_once('.') else {
            continue;
        };
        if key == "url" && !name.is_empty() {
            names.insert(name.to_owned());
        }
    }
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{
        configured_remote_names, configured_remote_names_in_config_order, configured_remotes,
        has_remote_section, is_valid_remote_name, remote_skip_fetch_all, ConfiguredRemote,
    };
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};
    use crate::remote::RemoteError;
    use crate::repo::{InitOptions, Repository};

    fn config_set(text: &str) -> ConfigSet {
        let file = ConfigFile::parse(std::path::Path::new("cfg"), text, ConfigScope::Local)
            .expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn configured_remotes_include_sections_without_url() {
        let set = config_set(
            "[remote \"zeta\"]\n\turl = /tmp/z.git\n[remote \"team.prod\"]\n\tfetch = +refs/heads/*:refs/remotes/team.prod/*\n[remote \"zeta\"]\n\turl = /tmp/z2.git\n",
        );
        assert_eq!(
            configured_remotes(&set),
            vec![
                ConfiguredRemote {
                    name: "team.prod".to_owned(),
                    url: None,
                    push_urls: vec![],
                    fetch_refspecs: vec!["+refs/heads/*:refs/remotes/team.prod/*".to_owned()],
                    effective_fetch_url: None,
                    effective_push_url: None,
                },
                ConfiguredRemote {
                    name: "zeta".to_owned(),
                    url: Some("/tmp/z2.git".to_owned()),
                    push_urls: vec![],
                    fetch_refspecs: vec![],
                    effective_fetch_url: Some("/tmp/z2.git".to_owned()),
                    effective_push_url: Some("/tmp/z2.git".to_owned()),
                },
            ]
        );
        assert_eq!(configured_remote_names(&set), vec!["zeta".to_owned()]);
        assert!(has_remote_section(&set, "team.prod"));
        assert!(has_remote_section(&set, "zeta"));
        assert!(!has_remote_section(&set, "team"));
    }

    #[test]
    fn remote_name_validation_follows_tracking_ref_rules() {
        for ok in ["origin", "team.prod", "a/b", "up-stream"] {
            assert!(is_valid_remote_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a..b",
            "has space",
            "x:y",
            "star*",
            "end.lock",
        ] {
            assert!(!is_valid_remote_name(bad), "{bad}");
        }
    }

    #[test]
    fn add_writes_url_and_default_fetch_refspec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        repo.remotes()
            .add("upstream", "https://example.com/r.git")
            .expect("add");
        let config = repo.config().expect("config");
        assert_eq!(
            config.get("remote.upstream.url").as_deref(),
            Some("https://example.com/r.git")
        );
        assert_eq!(
            config.get_all("remote.upstream.fetch"),
            vec!["+refs/heads/*:refs/remotes/upstream/*".to_owned()]
        );
        assert_eq!(repo.remotes().list().expect("list"), vec!["upstream"]);
    }

    #[test]
    fn add_rejects_existing_and_invalid_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        repo.remotes().add("origin", "/tmp/a.git").expect("add");
        assert!(matches!(
            repo.remotes().add("origin", "/tmp/b.git"),
            Err(RemoteError::RemoteExists(name)) if name == "origin"
        ));
        assert!(matches!(
            repo.remotes().add("bad name", "/tmp/b.git"),
            Err(RemoteError::InvalidRemoteName(name)) if name == "bad name"
        ));
        assert_eq!(
            repo.config()
                .expect("config")
                .get("remote.origin.url")
                .as_deref(),
            Some("/tmp/a.git")
        );
    }

    #[test]
    fn configured_remote_names_in_config_order_preserves_file_order() {
        let file = ConfigFile::parse(
            std::path::Path::new("cfg"),
            r#"[remote "second"]
	url = /tmp/second.git
[remote "first"]
	url = /tmp/first.git
"#,
            ConfigScope::Local,
        )
        .expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        assert_eq!(
            configured_remote_names_in_config_order(&set),
            vec!["second".to_owned(), "first".to_owned()]
        );
    }

    #[test]
    fn remote_skip_fetch_all_reads_config() {
        let file = ConfigFile::parse(
            std::path::Path::new("cfg"),
            r#"[remote "skipme"]
	url = /tmp/x.git
	skipFetchAll = true
"#,
            ConfigScope::Local,
        )
        .expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        assert!(remote_skip_fetch_all(&set, "skipme"));
        assert!(!remote_skip_fetch_all(&set, "other"));
    }

    #[test]
    fn configured_remote_names_parses_dotted_subsections() {
        let file = ConfigFile::parse(
            std::path::Path::new("cfg"),
            r#"[remote "team.prod"]
	url = /tmp/upstream.git
[remote "origin"]
	url = /tmp/origin.git
"#,
            ConfigScope::Local,
        )
        .expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        let names = configured_remote_names(&set);
        assert!(names.contains(&"team.prod".to_owned()));
        assert!(names.contains(&"origin".to_owned()));
    }
}
