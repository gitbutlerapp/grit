//! Snapshot checkouts: start work on a repository without cloning its history.
//!
//! A server that offers snapshot checkouts (`grit serve` does) answers under
//! its own endpoint, `/.grit/checkouts/<repository>`, separate from the Git
//! smart-HTTP paths, so a client can tell a server that has them from one that
//! doesn't. A checkout is one **snapshot commit**: the target branch's tree,
//! cut down to the paths asked for (as Git's cone-mode sparse checkout has
//! them: everything under each path, plus the files directly in every
//! directory above one), in a commit with fixed metadata whose message names
//! the real commit it stands for. Because the metadata is fixed, every
//! checkout of the same paths at the same commit gets the same snapshot.
//!
//! The server hands the snapshot out as a bundle, which the client downloads
//! with [`crate::resumable::download_resumable`] (so an interrupted checkout
//! resumes) and turns into a new repository whose history is that one commit,
//! with the snapshot at `refs/remotes/origin/<branch>` and a new work branch
//! on top of it checked out.
//!
//! - Wire types: [`CheckoutServerInfo`], [`CheckoutRequest`], [`CheckoutInfo`].
//! - Server side: [`normalize_paths`], [`filtered_tree`], [`snapshot_object`],
//!   [`make_snapshot`].
//! - Client side: [`CheckoutUrl`], [`CheckoutClient`], [`materialize`],
//!   [`CheckoutMarker`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fetch::Progress;
use crate::objects::{
    parse_commit, parse_tree, serialize_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry,
};
use crate::odb::Odb;
use crate::progress::Cancel;
use crate::repo::{InitOptions, Repository};
use crate::transport::http::{HttpClient, HttpMethod, HttpRequest};

/// Path under the server root where snapshot checkouts are served:
/// `/.grit/checkouts/<repository>`.
pub const CHECKOUT_API: &str = ".grit/checkouts";

/// Name and email of every snapshot commit's author and committer.
pub const SNAPSHOT_IDENT: &str = "grit-checkout <snapshot@grit-checkout>";

/// Trailer naming the real commit a snapshot stands for.
pub const SNAPSHOT_BASE_TRAILER: &str = "Grit-Checkout-Base";

/// File in the git directory that marks a repository as a snapshot checkout.
pub const MARKER_FILE: &str = "grit-checkout.json";

const TREE_MODE: u32 = 0o040000;
const GITLINK_MODE: u32 = 0o160000;

/// `GET /.grit/checkouts/<repo>`: what the server would check out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutServerInfo {
    /// The repository's name.
    pub name: String,
    /// The target branch checkouts start from.
    pub branch: String,
    /// Where the target branch is.
    pub head: String,
}

/// `POST /.grit/checkouts/<repo>`: make a checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRequest {
    /// Paths to check out; empty is the whole tree.
    #[serde(default)]
    pub paths: Vec<String>,
    /// What the client calls the checkout (its directory name), for the server's log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// The work branch name the client would like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
}

/// One snapshot commit of a checkout and the real commit it stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRoot {
    /// The snapshot commit.
    pub commit: String,
    /// The real commit of the target branch it stands for.
    pub base: String,
}

/// A checkout as the server knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutInfo {
    /// The checkout's id on the server.
    pub id: String,
    /// The repository's name, the default directory to check out into.
    pub name: String,
    /// The target branch.
    pub branch: String,
    /// The checkout's paths (normalized); empty is the whole tree.
    pub paths: Vec<String>,
    /// The real commit the newest snapshot stands for.
    pub base: String,
    /// The newest snapshot commit.
    pub root: String,
    /// That commit's exact bytes, so the client can check it rebuilt the same object.
    pub root_object: String,
    /// The snapshot's tree.
    pub tree: String,
    /// Every snapshot commit of the checkout, oldest first.
    pub roots: Vec<SnapshotRoot>,
    /// Files in the snapshot.
    pub files: u64,
    /// Bytes of those files.
    pub bytes: u64,
    /// Size of the bundle the snapshot downloads as.
    pub bundle_bytes: u64,
    /// The work branch the client should create: the one it asked for, numbered
    /// when the server already has a branch by that name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
}

/// An error body the checkout endpoints answer with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutErrorBody {
    /// What went wrong.
    pub error: String,
}

/// Why a snapshot checkout failed.
#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    /// The server has no snapshot-checkout endpoint for this repository.
    #[error("{url} offers no grit checkout (no {endpoint} endpoint); it may be a plain Git server, or the repository may not exist")]
    NotOffered {
        /// The URL the user gave.
        url: String,
        /// The endpoint that was missing.
        endpoint: String,
    },
    /// The server refused a request.
    #[error("{url}: {message} (HTTP {status})")]
    Server {
        /// The request URL.
        url: String,
        /// The response status.
        status: u16,
        /// The server's explanation.
        message: String,
    },
    /// The URL cannot name a repository.
    #[error("{0} is not an http(s) URL of a repository")]
    InvalidUrl(String),
    /// A path is not a plain path inside the repository.
    #[error("{path}: {reason}")]
    InvalidPath {
        /// The path as given.
        path: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A path is not in the tree being checked out.
    #[error("{path} is not in the repository at {base}")]
    MissingPath {
        /// The path.
        path: String,
        /// The commit it was looked up in.
        base: String,
    },
    /// The downloaded snapshot is not the one the server described.
    #[error("the downloaded snapshot is {got}, but the server described {expected}")]
    Mismatch {
        /// The snapshot commit the server described.
        expected: String,
        /// What the download held.
        got: String,
    },
    /// The destination already has files.
    #[error("{0} already exists and is not empty")]
    DestinationNotEmpty(PathBuf),
    /// The server's answer could not be parsed.
    #[error("unexpected answer from {url}: {detail}")]
    Protocol {
        /// The request URL.
        url: String,
        /// What was wrong.
        detail: String,
    },
    /// The download failed.
    #[error(transparent)]
    Download(Box<crate::resumable::ResumableError>),
    /// A repository operation failed.
    #[error(transparent)]
    Library(Box<crate::error::Error>),
    /// A file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<crate::error::Error> for CheckoutError {
    fn from(e: crate::error::Error) -> Self {
        Self::Library(Box::new(e))
    }
}

impl From<crate::resumable::ResumableError> for CheckoutError {
    fn from(e: crate::resumable::ResumableError) -> Self {
        Self::Download(Box::new(e))
    }
}

/// Result alias for snapshot checkouts.
pub type Result<T> = std::result::Result<T, CheckoutError>;

/// Clean up requested paths: relative, without `.`, `..` or empty components,
/// without trailing slashes, sorted, deduplicated, and with paths inside
/// another one dropped.
///
/// # Errors
///
/// [`CheckoutError::InvalidPath`] for an absolute path or one with `.`, `..`
/// or empty components.
pub fn normalize_paths(paths: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let trimmed = p.trim().trim_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        if p.trim().starts_with('/') {
            return Err(CheckoutError::InvalidPath {
                path: p.clone(),
                reason: "paths are relative to the repository root",
            });
        }
        if trimmed
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(CheckoutError::InvalidPath {
                path: p.clone(),
                reason: "not a plain path inside the repository",
            });
        }
        out.push(trimmed.to_owned());
    }
    out.sort();
    out.dedup();
    let all = out.clone();
    out.retain(|p| {
        !all.iter().any(|q| {
            q != p && p.starts_with(q.as_str()) && p.as_bytes().get(q.len()) == Some(&b'/')
        })
    });
    Ok(out)
}

/// The entry at `path` (slash-separated) under `tree`, as `(mode, oid)`.
fn lookup(odb: &Odb, tree: ObjectId, path: &[u8]) -> crate::error::Result<Option<(u32, ObjectId)>> {
    let mut current = (TREE_MODE, tree);
    for component in path.split(|b| *b == b'/') {
        if current.0 != TREE_MODE {
            return Ok(None);
        }
        let entries = parse_tree(&odb.read(&current.1)?.data)?;
        match entries.into_iter().find(|e| e.name == component) {
            Some(e) => current = (e.mode, e.oid),
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// A tree being assembled from entries at full paths.
#[derive(Default)]
struct Dir {
    entries: BTreeMap<Vec<u8>, Node>,
}

enum Node {
    Entry(u32, ObjectId),
    Dir(Dir),
}

impl Dir {
    fn insert(&mut self, path: &[u8], mode: u32, oid: ObjectId) {
        match path.iter().position(|b| *b == b'/') {
            None => {
                self.entries.insert(path.to_vec(), Node::Entry(mode, oid));
            }
            Some(i) => {
                let node = self
                    .entries
                    .entry(path[..i].to_vec())
                    .or_insert_with(|| Node::Dir(Dir::default()));
                if let Node::Dir(dir) = node {
                    dir.insert(&path[i + 1..], mode, oid);
                }
            }
        }
    }

    fn write(self, odb: &Odb) -> crate::error::Result<ObjectId> {
        let mut entries = Vec::with_capacity(self.entries.len());
        for (name, node) in self.entries {
            let (mode, oid) = match node {
                Node::Entry(mode, oid) => (mode, oid),
                Node::Dir(dir) => (TREE_MODE, dir.write(odb)?),
            };
            entries.push(TreeEntry { mode, name, oid });
        }
        entries.sort_by(|a, b| {
            tree_entry_cmp(&a.name, a.mode == TREE_MODE, &b.name, b.mode == TREE_MODE)
        });
        odb.write(ObjectKind::Tree, &serialize_tree(&entries))
    }
}

/// `tree` cut down to `paths`: each path's entry where it is, and the files
/// directly in every directory above one, the top level included, as Git's
/// cone-mode sparse checkout has them. Selected directories' trees are reused
/// as they are, so only the root and the directories above nested paths are
/// written. No paths is the whole tree.
///
/// # Errors
///
/// [`CheckoutError::MissingPath`] when `tree` has no entry at one of `paths`,
/// and object read or write failures.
pub fn filtered_tree(odb: &Odb, tree: ObjectId, paths: &[String]) -> Result<ObjectId> {
    if paths.is_empty() {
        return Ok(tree);
    }
    let mut dirs: Vec<&[u8]> = vec![b""];
    for p in paths {
        let p = p.as_bytes();
        dirs.extend(
            p.iter()
                .enumerate()
                .filter(|(_, b)| **b == b'/')
                .map(|(i, _)| &p[..i]),
        );
    }
    dirs.sort();
    dirs.dedup();
    let mut root = Dir::default();
    for dir in dirs {
        let oid = if dir.is_empty() {
            tree
        } else {
            match lookup(odb, tree, dir)? {
                Some((TREE_MODE, oid)) => oid,
                _ => continue,
            }
        };
        for entry in parse_tree(&odb.read(&oid)?.data)? {
            if entry.mode == TREE_MODE {
                continue;
            }
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                [dir, b"/", &entry.name].concat()
            };
            root.insert(&path, entry.mode, entry.oid);
        }
    }
    for p in paths {
        let Some((mode, oid)) = lookup(odb, tree, p.as_bytes())? else {
            return Err(CheckoutError::MissingPath {
                path: p.clone(),
                base: tree.to_hex(),
            });
        };
        root.insert(p.as_bytes(), mode, oid);
    }
    Ok(root.write(odb)?)
}

/// The bytes of the snapshot commit standing for `base` with tree `tree` on
/// `parent`.
///
/// The author and committer are [`SNAPSHOT_IDENT`] at `base_time` (UTC), and
/// the message names `base`, repeats its message, and ends with a
/// [`SNAPSHOT_BASE_TRAILER`] trailer. Nothing else goes in, so the same inputs
/// always give the same commit.
#[must_use]
pub fn snapshot_object(
    tree: ObjectId,
    parent: Option<ObjectId>,
    base: ObjectId,
    base_time: i64,
    base_message: &str,
) -> Vec<u8> {
    let base = base.to_hex();
    let who = format!("{SNAPSHOT_IDENT} {base_time} +0000");
    let mut s = format!("tree {}\n", tree.to_hex());
    if let Some(p) = parent {
        s.push_str(&format!("parent {}\n", p.to_hex()));
    }
    let subject = base_message.lines().next().unwrap_or_default().trim();
    let quoted: String = base_message
        .trim_end()
        .lines()
        .map(|l| format!("    {l}").trim_end().to_owned() + "\n")
        .collect();
    s.push_str(&format!(
        "author {who}\ncommitter {who}\n\nSnapshot of {}: {subject}\n\nStands for {base}:\n\n{quoted}\n{SNAPSHOT_BASE_TRAILER}: {base}\n",
        &base[..base.len().min(12)]
    ));
    s.into_bytes()
}

/// A snapshot commit written into a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The snapshot commit.
    pub commit: ObjectId,
    /// Its tree.
    pub tree: ObjectId,
    /// The real commit it stands for.
    pub base: ObjectId,
    /// The commit's exact bytes.
    pub object: Vec<u8>,
    /// Files in the tree (submodules not counted).
    pub files: u64,
    /// Bytes of those files.
    pub bytes: u64,
}

/// Write the snapshot of `base` filtered to `paths` (see [`filtered_tree`])
/// on `parent` into `repo`, with the number and size of its files.
///
/// # Errors
///
/// [`CheckoutError::MissingPath`] for a path `base` lacks, and object read or
/// write failures.
pub fn make_snapshot(
    repo: &Repository,
    base: ObjectId,
    paths: &[String],
    parent: Option<ObjectId>,
) -> Result<Snapshot> {
    let odb = repo.odb();
    let commit = parse_commit(&odb.read(&base)?.data)?;
    let tree = filtered_tree(odb, commit.tree, paths).map_err(|e| match e {
        CheckoutError::MissingPath { path, .. } => CheckoutError::MissingPath {
            path,
            base: base.to_hex(),
        },
        other => other,
    })?;
    let time = crate::ident::parse_signature_times(&commit.committer)
        .map(|t| t.unix_seconds)
        .unwrap_or(0);
    let object = snapshot_object(tree, parent, base, time, &commit.message);
    let id = odb.write(ObjectKind::Commit, &object)?;
    let (files, bytes) = tree_stats(odb, tree)?;
    Ok(Snapshot {
        commit: id,
        tree,
        base,
        object,
        files,
        bytes,
    })
}

/// Files under `tree` and their total size, without reading file contents.
fn tree_stats(odb: &Odb, tree: ObjectId) -> crate::error::Result<(u64, u64)> {
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut stack = vec![tree];
    while let Some(oid) = stack.pop() {
        for entry in parse_tree(&odb.read(&oid)?.data)? {
            match entry.mode {
                TREE_MODE => stack.push(entry.oid),
                GITLINK_MODE => {}
                _ => {
                    files += 1;
                    bytes += odb.read_info(&entry.oid)?.size;
                }
            }
        }
    }
    Ok((files, bytes))
}

/// A repository URL split into what snapshot checkouts need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutUrl {
    /// The repository URL, for Git (`origin`).
    pub repo_url: String,
    /// The repository's name (last path segment, without `.git`).
    pub name: String,
    /// The checkout endpoint, `<server>/.grit/checkouts/<path>`.
    pub api: String,
}

impl CheckoutUrl {
    /// Split `url` (`http(s)://host[:port]/<path to repository>[.git]`).
    ///
    /// # Errors
    ///
    /// [`CheckoutError::InvalidUrl`] when `url` is not http(s) or names no
    /// repository.
    pub fn parse(url: &str) -> Result<Self> {
        let invalid = || CheckoutError::InvalidUrl(url.to_owned());
        let parsed = url::Url::parse(url).map_err(|_| invalid())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(invalid());
        }
        let path = parsed.path().trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if path.is_empty() {
            return Err(invalid());
        }
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let mut server = parsed.clone();
        server.set_path("");
        server.set_query(None);
        server.set_fragment(None);
        let server = server.as_str().trim_end_matches('/').to_owned();
        Ok(Self {
            repo_url: format!("{server}/{path}.git"),
            name,
            api: format!("{server}/{CHECKOUT_API}/{path}"),
        })
    }
}

/// Talks to a server's snapshot-checkout endpoint.
pub struct CheckoutClient<'a> {
    http: &'a dyn HttpClient,
    url: CheckoutUrl,
    given: String,
}

impl<'a> CheckoutClient<'a> {
    /// A client for the repository at `url`.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::InvalidUrl`] as for [`CheckoutUrl::parse`].
    pub fn new(http: &'a dyn HttpClient, url: &str) -> Result<Self> {
        Ok(Self {
            http,
            url: CheckoutUrl::parse(url)?,
            given: url.to_owned(),
        })
    }

    /// The parsed URL.
    #[must_use]
    pub fn url(&self) -> &CheckoutUrl {
        &self.url
    }

    fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<&[u8]>,
    ) -> Result<T> {
        let mut request = HttpRequest::new(method, url).header("Accept", "application/json");
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body);
        }
        let response = self.http.send(&request)?;
        let status = response.status;
        let bytes = response.into_bytes()?;
        if status == 404 && url == self.url.api {
            return Err(CheckoutError::NotOffered {
                url: self.given.clone(),
                endpoint: self.url.api.clone(),
            });
        }
        if !(200..300).contains(&status) {
            let message = serde_json::from_slice::<CheckoutErrorBody>(&bytes)
                .map(|b| b.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).trim().to_owned());
            return Err(CheckoutError::Server {
                url: url.to_owned(),
                status,
                message,
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| CheckoutError::Protocol {
            url: url.to_owned(),
            detail: e.to_string(),
        })
    }

    /// What the server would check out.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::NotOffered`] when the server has no checkout endpoint
    /// for the repository, and request failures.
    pub fn server_info(&self) -> Result<CheckoutServerInfo> {
        self.json(HttpMethod::Get, &self.url.api, None)
    }

    /// Make a checkout on the server.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::NotOffered`], and the server's refusal (a path it
    /// doesn't have, an empty repository).
    pub fn create(&self, request: &CheckoutRequest) -> Result<CheckoutInfo> {
        let body = serde_json::to_vec(request).map_err(|e| CheckoutError::Protocol {
            url: self.url.api.clone(),
            detail: e.to_string(),
        })?;
        self.json(HttpMethod::Post, &self.url.api, Some(&body))
    }

    /// The URL of checkout `id`'s bundle.
    #[must_use]
    pub fn bundle_url(&self, id: &str) -> String {
        format!("{}/{id}/bundle", self.url.api)
    }
}

/// What makes a repository a snapshot checkout: where it came from and which
/// snapshot commits it has. Kept as [`MARKER_FILE`] in the git directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutMarker {
    /// The repository URL it was checked out from.
    pub url: String,
    /// The checkout's id on the server.
    pub id: String,
    /// The target branch.
    pub branch: String,
    /// The checkout's paths.
    pub paths: Vec<String>,
    /// The snapshot commits, oldest first.
    pub roots: Vec<SnapshotRoot>,
}

impl CheckoutMarker {
    /// Read the marker of the repository whose git directory is `git_dir`.
    ///
    /// # Returns
    ///
    /// `None` when it is not a snapshot checkout.
    #[must_use]
    pub fn read(git_dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(git_dir.join(MARKER_FILE)).ok()?).ok()
    }

    /// Write it into `git_dir`.
    ///
    /// # Errors
    ///
    /// File write failures.
    pub fn write(&self, git_dir: &Path) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|e| std::io::Error::other(e.to_string()))?;
        let tmp = git_dir.join(format!("{MARKER_FILE}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, git_dir.join(MARKER_FILE))?;
        Ok(())
    }
}

/// A checkout built on disk.
#[derive(Debug)]
pub struct Materialized {
    /// The new repository.
    pub repo: Repository,
    /// The work branch checked out.
    pub work: String,
    /// Files written to the work tree.
    pub files: u64,
}

/// Build the checkout described by `info` in `dest` from its downloaded
/// `bundle`.
///
/// Creates the repository, unpacks the bundle, checks that the snapshot
/// commit is byte for byte the one the server described, points
/// `refs/remotes/origin/<branch>` and the new branch `work` at it, configures
/// `origin` as `repo_url`, writes the [`CheckoutMarker`] and checks out the
/// files.
///
/// # Errors
///
/// [`CheckoutError::DestinationNotEmpty`], [`CheckoutError::Mismatch`] for a
/// download that is not the described snapshot, and repository failures.
#[allow(clippy::too_many_arguments)]
pub fn materialize(
    dest: &Path,
    info: &CheckoutInfo,
    bundle: &Path,
    repo_url: &str,
    work: &str,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<Materialized> {
    if dest.is_dir() && std::fs::read_dir(dest)?.next().is_some() {
        return Err(CheckoutError::DestinationNotEmpty(dest.to_path_buf()));
    }
    std::fs::create_dir_all(dest)?;
    let repo = Repository::init(
        dest,
        &InitOptions {
            initial_branch: Some(work.to_owned()),
            ..InitOptions::default()
        },
    )?;
    let opened = repo
        .bundles()
        .open(bundle)
        .map_err(|e| crate::error::Error::Message(format!("bundle: {e}")))?;
    let unbundled = opened
        .unbundle_with_progress(&repo, progress, cancel)
        .map_err(|e| crate::error::Error::Message(format!("bundle: {e}")))?;
    let mismatch = |got: String| CheckoutError::Mismatch {
        expected: info.root.clone(),
        got,
    };
    let root = ObjectId::from_hex(&info.root).map_err(|_| mismatch(info.root.clone()))?;
    let tip = unbundled
        .refs
        .first()
        .map(|r| r.oid)
        .ok_or_else(|| mismatch("an empty bundle".to_owned()))?;
    if tip != root {
        return Err(mismatch(tip.to_hex()));
    }
    let object = repo.odb().read(&root)?;
    if object.data != info.root_object.as_bytes() {
        return Err(mismatch(format!(
            "{} with different contents",
            root.to_hex()
        )));
    }
    let git_dir = repo.git_dir().to_path_buf();
    crate::refs::write_ref(
        &git_dir,
        &format!("refs/remotes/origin/{}", info.branch),
        &root,
    )?;
    crate::refs::write_ref(&git_dir, &format!("refs/heads/{work}"), &root)?;
    crate::refs::write_symbolic_ref(&git_dir, "HEAD", &format!("refs/heads/{work}"))?;
    let config_path = git_dir.join("config");
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut config = crate::config::ConfigFile::parse(
        &config_path,
        &content,
        crate::config::ConfigScope::Local,
    )?;
    config.set("remote.origin.url", repo_url)?;
    config.set("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")?;
    config.write()?;
    CheckoutMarker {
        url: repo_url.to_owned(),
        id: info.id.clone(),
        branch: info.branch.clone(),
        paths: info.paths.clone(),
        roots: info.roots.clone(),
    }
    .write(&git_dir)?;
    let tree = ObjectId::from_hex(&info.tree).map_err(|_| mismatch(info.tree.clone()))?;
    crate::porcelain::checkout::checkout_between_trees_with_progress(&repo, None, &tree, progress)?;
    let files = u64::try_from(repo.index_handle().load()?.entries().len()).unwrap_or(u64::MAX);
    Ok(Materialized {
        repo,
        work: work.to_owned(),
        files,
    })
}

const WORDS: &[&str] = &[
    "amber", "brook", "cedar", "dune", "ember", "fern", "glade", "harbor", "iris", "juniper",
    "kestrel", "lagoon", "maple", "nectar", "orchid", "pine", "quartz", "river", "sage", "thistle",
    "umber", "violet", "willow", "yarrow", "zephyr", "aspen", "birch", "coral", "delta", "elm",
    "flint", "grove", "hazel", "inlet", "jasper", "lark", "moss", "north", "oak", "pebble",
    "quill", "reed", "slate", "tide", "vale", "wren",
];

/// A work branch name for a new checkout: the local part of `email` and two
/// words chosen by `random`, like `alice/quiet-river`.
#[must_use]
pub fn suggest_work_branch(email: Option<&str>, random: [u8; 2]) -> String {
    let who: String = email
        .and_then(|e| e.split('@').next())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let who = who.trim_matches('-');
    let a = WORDS[usize::from(random[0]) % WORDS.len()];
    let b = WORDS[usize::from(random[1]) % WORDS.len()];
    if who.is_empty() {
        format!("checkout/{a}-{b}")
    } else {
        format!("{who}/{a}-{b}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized() {
        let paths: Vec<String> = ["b/", "a/x", "a", " c ", "", "a/y"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            normalize_paths(&paths).ok(),
            Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
        );
        assert!(normalize_paths(&["/abs".to_owned()]).is_err());
        assert!(normalize_paths(&["a/../b".to_owned()]).is_err());
        assert!(normalize_paths(&["a//b".to_owned()]).is_err());
    }

    #[test]
    fn urls_split_into_repository_and_endpoint() {
        let u = CheckoutUrl::parse("http://h:8080/demo.git").ok();
        assert_eq!(
            u,
            Some(CheckoutUrl {
                repo_url: "http://h:8080/demo.git".to_owned(),
                name: "demo".to_owned(),
                api: "http://h:8080/.grit/checkouts/demo".to_owned(),
            })
        );
        let nested = CheckoutUrl::parse("https://h/org/app/").ok();
        assert_eq!(nested.as_ref().map(|u| u.name.as_str()), Some("app"));
        assert_eq!(
            nested.map(|u| u.api),
            Some("https://h/.grit/checkouts/org/app".to_owned())
        );
        assert!(CheckoutUrl::parse("http://h/").is_err());
        assert!(CheckoutUrl::parse("ssh://h/demo").is_err());
    }

    #[test]
    fn snapshot_objects_are_deterministic() {
        let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").ok();
        let base = ObjectId::from_hex("1111111111111111111111111111111111111111").ok();
        let (Some(tree), Some(base)) = (tree, base) else {
            panic!("oids");
        };
        let a = snapshot_object(tree, None, base, 1_700_000_000, "Fix it\n\nBody\n");
        assert_eq!(
            a,
            snapshot_object(tree, None, base, 1_700_000_000, "Fix it\n\nBody\n")
        );
        let text = String::from_utf8_lossy(&a);
        assert!(text.starts_with("tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor grit-checkout <snapshot@grit-checkout> 1700000000 +0000\n"), "{text}");
        assert!(
            text.contains("\n\nSnapshot of 111111111111: Fix it\n"),
            "{text}"
        );
        assert!(
            text.ends_with("Grit-Checkout-Base: 1111111111111111111111111111111111111111\n"),
            "{text}"
        );
    }

    #[test]
    fn work_branches_are_named_for_the_committer() {
        assert_eq!(
            suggest_work_branch(Some("Alice.B@example.com"), [0, 1]),
            "alice-b/amber-brook"
        );
        assert_eq!(suggest_work_branch(None, [2, 3]), "checkout/cedar-dune");
    }
}
