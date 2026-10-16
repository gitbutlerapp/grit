//! Remote configuration and transport operations scoped to a [`Repository`](crate::repo::Repository).

use std::collections::BTreeSet;

use crate::config::ConfigSet;
use crate::error::Result;
use crate::fetch::Progress;
use crate::remote::{
    HttpClientFactory, ListRefsOptions, Remote, RemoteError, RemoteRef, DEFAULT_REMOTE,
};
use crate::repo::Repository;
use crate::transfer::{FetchOptions, FetchOutcome, PushOptions, PushOutcome, PushRefSpec};

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
    use super::configured_remote_names;
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};

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
