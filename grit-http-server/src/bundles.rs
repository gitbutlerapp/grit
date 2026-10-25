//! Prebuilt clone bundles for `grit serve`.
//!
//! Each repository gets a bundle of its branches and tags, kept in
//! `<git-dir>/grit-serve/bundles/` and offered to cloning clients through the
//! protocol v2 `bundle-uri` command. A client downloads it as a plain, resumable
//! file (`GET /<repo>/bundles/<file>`, with `Range` support) and then fetches
//! only what is newer.
//!
//! A [`BundleScheduler`] thread builds a repository's first bundle as soon as
//! the server starts (or as soon as the repository has a branch), and builds a
//! new one when the refs have changed and the current bundle is older than
//! [`BundleSettings::interval`] (24 hours unless configured). Each bundle has a
//! new file name, so a client resuming a download never splices two versions;
//! the previous bundle is kept for clients still downloading it.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use grit_lib::bundle::BundleSpec;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::remote::bundle_uri::{BundleList, BundleListEntry, BundleListMode};
use grit_lib::repo::Repository;
use grit_protocol::RepositoryOptions;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// How often a new bundle is built for a repository that keeps changing.
pub const DEFAULT_BUNDLE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How often the scheduler looks for repositories that need a bundle.
pub const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(60);

/// URL path segment bundles are served under: `/<repo>/bundles/<file>`.
pub const BUNDLES_PATH: &str = "bundles";

/// Name of the manifest describing a repository's current bundle.
const MANIFEST: &str = "current.json";

/// When bundles are built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleSettings {
    /// Build and offer bundles at all.
    pub enabled: bool,
    /// Rebuild a changed repository's bundle once it is this old.
    pub interval: Duration,
    /// How often to look for work.
    pub check_every: Duration,
}

impl Default for BundleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: DEFAULT_BUNDLE_INTERVAL,
            check_every: DEFAULT_CHECK_EVERY,
        }
    }
}

/// Errors from building a bundle.
#[derive(Debug, thiserror::Error)]
pub enum BundleBuildError {
    /// Opening or reading the repository failed.
    #[error(transparent)]
    Repository(#[from] grit_lib::error::Error),
    /// Writing the bundle failed.
    #[error(transparent)]
    Bundle(#[from] grit_lib::bundle::BundleError),
    /// Writing files failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The bundle a repository currently offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleManifest {
    /// File name inside the bundle directory.
    pub file: String,
    /// When it was built, in Unix seconds.
    pub created: i64,
    /// Digest of the refs it holds ([`refs_digest`]).
    pub refs_digest: String,
    /// Size in bytes.
    pub size: u64,
}

/// The directory a repository's bundles live in.
#[must_use]
pub fn bundle_dir(repo_path: &Path) -> PathBuf {
    let dot_git = repo_path.join(".git");
    let git_dir = if dot_git.join("HEAD").is_file() {
        dot_git
    } else {
        repo_path.to_path_buf()
    };
    git_dir.join("grit-serve").join("bundles")
}

/// The current bundle of the repository at `repo_path`, if one was built.
#[must_use]
pub fn current(repo_path: &Path) -> Option<BundleManifest> {
    let dir = bundle_dir(repo_path);
    let manifest: BundleManifest =
        serde_json::from_slice(&std::fs::read(dir.join(MANIFEST)).ok()?).ok()?;
    dir.join(&manifest.file).is_file().then_some(manifest)
}

/// Whether `file` is a bundle file name this server could have written, and
/// still has.
#[must_use]
pub fn served_file(repo_path: &Path, file: &str) -> Option<PathBuf> {
    let safe = file.len() <= 96
        && file.ends_with(".bundle")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !file.starts_with('.');
    if !safe {
        return None;
    }
    let path = bundle_dir(repo_path).join(file);
    path.is_file().then_some(path)
}

/// The branches and tags a bundle holds, sorted by name.
fn bundle_refs(repo: &Repository) -> Result<Vec<(String, ObjectId)>, BundleBuildError> {
    let mut refs = Vec::new();
    for prefix in ["refs/heads/", "refs/tags/"] {
        refs.extend(grit_lib::refs::list_refs(repo.git_dir(), prefix)?);
    }
    refs.sort();
    Ok(refs)
}

/// A digest of the repository's branches and tags, or `None` when it has none
/// (there is nothing to bundle).
///
/// # Errors
///
/// Ref listing failures.
pub fn refs_digest(repo: &Repository) -> Result<Option<String>, BundleBuildError> {
    let refs = bundle_refs(repo)?;
    if refs.is_empty() {
        return Ok(None);
    }
    let mut hasher = grit_lib::objects::hash::ChecksumHasher::new(HashAlgo::Sha256);
    for (name, oid) in &refs {
        hasher.update(format!("{} {name}\n", oid.to_hex()).as_bytes());
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

/// Whether a repository whose refs digest to `digest` needs a new bundle at
/// `now`, given its `current` one.
#[must_use]
pub fn needs_bundle(
    current: Option<&BundleManifest>,
    digest: Option<&str>,
    now: OffsetDateTime,
    interval: Duration,
) -> bool {
    let Some(digest) = digest else {
        return false;
    };
    let Some(current) = current else {
        return true;
    };
    if current.refs_digest == digest {
        return false;
    }
    let age = now.unix_timestamp().saturating_sub(current.created);
    u64::try_from(age).is_ok_and(|age| age >= interval.as_secs())
}

/// Build a new bundle for the repository at `repo_path` and make it current.
///
/// Keeps the previous bundle (clients may still be downloading it) and removes
/// older ones.
///
/// # Returns
///
/// The new manifest, or `None` when the repository has no branches or tags.
///
/// # Errors
///
/// Repository, bundle and file failures. A failed build leaves the current
/// bundle in place.
pub fn build(
    repo_path: &Path,
    options: &RepositoryOptions,
    now: OffsetDateTime,
) -> Result<Option<BundleManifest>, BundleBuildError> {
    let repo = Repository::open(repo_path, &options.clone().for_serving())?;
    let refs = bundle_refs(&repo)?;
    let Some(digest) = refs_digest(&repo)? else {
        return Ok(None);
    };
    let dir = bundle_dir(repo_path);
    std::fs::create_dir_all(&dir)?;
    let created = now.unix_timestamp();
    let file = format!("{created}-{}.bundle", &digest[..12]);
    let tmp = dir.join(format!(".{file}.tmp"));
    let spec = BundleSpec {
        include: refs
            .iter()
            .map(|(name, oid)| (*oid, name.clone()))
            .collect(),
        exclude: Vec::new(),
        filter: None,
    };
    {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        grit_lib::bundle::write_bundle(&repo, &spec, &mut out)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(&file))?;
    let size = std::fs::metadata(dir.join(&file))?.len();
    let previous = current(repo_path).map(|m| m.file);
    let manifest = BundleManifest {
        file: file.clone(),
        created,
        refs_digest: digest,
        size,
    };
    let manifest_tmp = dir.join(format!(".{MANIFEST}.tmp"));
    let bytes = serde_json::to_vec(&manifest).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(&manifest_tmp, bytes)?;
    std::fs::rename(&manifest_tmp, dir.join(MANIFEST))?;
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let keep = name == file || Some(&name) == previous.as_ref() || name == MANIFEST;
        if !keep && name.ends_with(".bundle") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(Some(manifest))
}

/// The bundle list to advertise for repository `name`, whose current bundle is
/// `manifest`, on a server reached at `base_url` (`http://host:port`).
#[must_use]
pub fn bundle_list(manifest: &BundleManifest, base_url: &str, name: &str) -> BundleList {
    BundleList {
        mode: BundleListMode::All,
        bundles: vec![BundleListEntry {
            id: "clone".to_owned(),
            uri: format!(
                "{}/{name}/{BUNDLES_PATH}/{}",
                base_url.trim_end_matches('/'),
                manifest.file
            ),
            // A single full bundle needs no `creationToken` heuristic, and Git
            // (2.50) skips advertised lists that use it during a clone.
            creation_token: None,
        }],
    }
}

/// The background thread that keeps every repository's bundle current.
///
/// Dropping the scheduler stops the thread after its current check.
pub struct BundleScheduler {
    wake: mpsc::Sender<()>,
}

impl BundleScheduler {
    /// Start checking the repositories `list` returns, now and every
    /// [`BundleSettings::check_every`].
    ///
    /// `on_built` is told about each bundle built, and `on_error` about each
    /// failure (the server logs them).
    pub fn start(
        settings: BundleSettings,
        options: RepositoryOptions,
        list: impl Fn() -> Vec<PathBuf> + Send + 'static,
        on_built: impl Fn(&Path, &BundleManifest) + Send + 'static,
        on_error: impl Fn(&Path, &BundleBuildError) + Send + 'static,
    ) -> Self {
        let (wake, wakes) = mpsc::channel();
        std::thread::spawn(move || loop {
            for repo_path in list() {
                let now = OffsetDateTime::now_utc();
                let digest = Repository::open(&repo_path, &options.clone().for_serving())
                    .map_err(BundleBuildError::from)
                    .and_then(|repo| refs_digest(&repo));
                let digest = match digest {
                    Ok(d) => d,
                    Err(e) => {
                        on_error(&repo_path, &e);
                        continue;
                    }
                };
                let current = current(&repo_path);
                if !needs_bundle(current.as_ref(), digest.as_deref(), now, settings.interval) {
                    continue;
                }
                match build(&repo_path, &options, now) {
                    Ok(Some(manifest)) => on_built(&repo_path, &manifest),
                    Ok(None) => {}
                    Err(e) => on_error(&repo_path, &e),
                }
            }
            match wakes.recv_timeout(settings.check_every) {
                Ok(()) => while wakes.try_recv().is_ok() {},
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        });
        Self { wake }
    }

    /// Check again now, for example after a push.
    pub fn wake(&self) {
        let _ = self.wake.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(created: i64, digest: &str) -> BundleManifest {
        BundleManifest {
            file: format!("{created}-x.bundle"),
            created,
            refs_digest: digest.to_owned(),
            size: 1,
        }
    }

    #[test]
    fn bundles_are_built_first_then_after_changes_once_old_enough() {
        let day = Duration::from_secs(86_400);
        let at =
            |secs| OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        assert!(!needs_bundle(None, None, at(0), day), "nothing to bundle");
        assert!(needs_bundle(None, Some("a"), at(0), day), "first bundle");
        let current = manifest(1_000, "a");
        assert!(!needs_bundle(Some(&current), Some("a"), at(1_000_000), day));
        assert!(!needs_bundle(
            Some(&current),
            Some("b"),
            at(1_000 + 3_600),
            day
        ));
        assert!(needs_bundle(
            Some(&current),
            Some("b"),
            at(1_000 + 86_400),
            day
        ));
        assert!(needs_bundle(
            Some(&current),
            Some("b"),
            at(1_000),
            Duration::ZERO
        ));
    }

    #[test]
    fn only_bundle_file_names_are_served() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("tempdir: {e}"),
        };
        let bundles = bundle_dir(dir.path());
        assert!(std::fs::create_dir_all(&bundles).is_ok());
        assert!(std::fs::write(bundles.join("1-ab.bundle"), b"x").is_ok());
        assert!(std::fs::write(bundles.join(MANIFEST), b"{}").is_ok());
        assert!(served_file(dir.path(), "1-ab.bundle").is_some());
        assert!(served_file(dir.path(), "2-ab.bundle").is_none());
        assert!(served_file(dir.path(), MANIFEST).is_none());
        assert!(served_file(dir.path(), "../HEAD").is_none());
        assert!(served_file(dir.path(), "..bundle").is_none());
    }

    #[test]
    fn bundle_lists_point_at_the_served_file() {
        let list = bundle_list(&manifest(7, "a"), "http://h:1/", "repo");
        assert_eq!(list.bundles.len(), 1);
        assert_eq!(list.bundles[0].uri, "http://h:1/repo/bundles/7-x.bundle");
        assert_eq!(list.bundles[0].creation_token, None);
    }
}
