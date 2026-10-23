//! Clone a remote repository: initialize, configure `origin`, fetch, and set up the default branch.
//!
//! File checkout is left to the caller (typically [`crate::porcelain::checkout`]).

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::environment::Environment;
use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::fetch::{fetch_operation_identity, Progress};
use crate::objects::ObjectId;
use crate::ref_storage::RefStorageFormat;
use crate::refs;
use crate::remote::{HttpClientFactory, ListRefsOptions, Remote, RemoteError, DEFAULT_REMOTE};
use crate::repo::{init_repository, Repository};
use crate::transfer::{CloneReflog, FetchOptions, TagMode, TransferStats};
use crate::transport_path::{
    absolute_local_clone_source_url, git_url_basename, resolve_local_remote_git_dir,
    should_store_absolute_local_clone_url, url_is_local_not_ssh, NoDirectoryName,
};

/// Errors during [`clone`].
#[derive(Debug, Error)]
pub enum CloneError {
    /// Destination exists and is not empty.
    ///
    /// Grit does not create or modify the destination in this case; callers must not run
    /// [`cleanup_failed_clone`] on the path.
    #[error("destination '{0}' already exists and is not empty")]
    DestNotEmpty(String),
    /// Underlying library failure.
    #[error(transparent)]
    Library(#[from] Error),
    /// Remote resolution or transport failure.
    #[error(transparent)]
    Remote(#[from] crate::remote::RemoteError),
}

impl CloneError {
    /// Whether [`cleanup_failed_clone`] may run after this failure.
    ///
    /// Returns `false` for [`CloneError::DestNotEmpty`], which rejects the destination before
    /// any repository is initialized.
    #[must_use]
    pub fn should_cleanup_failed_clone(&self) -> bool {
        !matches!(self, Self::DestNotEmpty(_))
    }

    /// Whether the failure is a transport/read error (not an empty remote with no branches).
    #[must_use]
    pub fn is_remote_read_failure(&self) -> bool {
        match self {
            Self::Library(e) => matches!(e, Error::RemoteReadFailed),
            Self::Remote(crate::remote::RemoteError::Library(e)) => {
                matches!(e, Error::RemoteReadFailed)
            }
            Self::DestNotEmpty(_) | Self::Remote(_) => false,
        }
    }
}

/// Which tags to fetch during [`clone`], matching `git clone` tag behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneTags {
    /// Fetch every tag on the remote (`git fetch --tags` during clone).
    All,
    /// Fetch tags reachable from fetched commits (default for shallow clones).
    Following,
    /// Do not fetch tags; writes `remote.<name>.tagOpt=--no-tags`.
    None,
}

impl CloneTags {
    /// Maps to [`TagMode`] for the initial fetch.
    #[must_use]
    pub const fn tag_mode(self) -> TagMode {
        match self {
            Self::All => TagMode::All,
            Self::Following => TagMode::Following,
            Self::None => TagMode::None,
        }
    }
}

/// Options for [`clone`].
pub struct CloneOptions {
    /// Source URL or path (as passed on the command line).
    pub url: String,
    /// Directory to create or use for the new repository.
    pub dest: PathBuf,
    /// Remote name to configure (default [`DEFAULT_REMOTE`]).
    pub remote_name: String,
    /// Process environment for config and reflog identity.
    pub environment: Environment,
    /// Initial branch name used when initializing the empty repo (`main`).
    pub initial_branch: String,
    /// Global thread policy for fetch pack ingest and the opened repository (`--threads`).
    pub thread_policy: crate::parallel::ThreadPolicy,
    /// Truncate fetched history to this many commits per ref tip
    /// (`git clone --depth N`). `Some(1)` matches a shallow clone.
    pub depth: Option<u32>,
    /// Branch to check out (`git clone -b`), when set.
    pub branch: Option<String>,
    /// Single-branch fetch (`git clone --single-branch` / `--no-single-branch`).
    ///
    /// `None` matches Git: depth implies single-branch unless explicitly disabled.
    pub single_branch: Option<bool>,
    /// Tag fetch mode. `None` uses Git defaults (all tags for full clones, following for shallow).
    pub tags: Option<CloneTags>,
}

impl CloneOptions {
    /// Options for cloning into `dest` from `url`, using the current process environment.
    #[must_use]
    pub fn new(url: impl Into<String>, dest: PathBuf) -> Self {
        Self {
            url: url.into(),
            dest,
            remote_name: DEFAULT_REMOTE.to_owned(),
            environment: Environment::capture_process(), // hygiene: CLI boundary — snapshot process environment for clone
            initial_branch: "main".to_owned(),
            thread_policy: crate::parallel::ThreadPolicy::default(),
            depth: None,
            branch: None,
            single_branch: None,
            tags: None,
        }
    }
}

/// Whether [`clone`] checked out the default branch or left the work tree empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneCheckout {
    /// The default branch tip was fetched and should be checked out.
    CheckedOut(ObjectId),
    /// No objects and no remote branches were received.
    EmptyRemote,
    /// The remote advertised default branch has no commits yet; other refs may exist.
    UnbornDefaultBranch,
}

/// Result of a successful [`clone`] before working-tree checkout.
#[derive(Debug)]
pub struct CloneOutcome {
    /// Source URL string from the options.
    pub url: String,
    /// Destination path.
    pub path: PathBuf,
    /// Short name of the default branch that was created.
    pub branch: String,
    /// Commit to check out on that branch.
    ///
    /// [`ObjectId::zero()`] when the remote default branch has no commits yet.
    pub checkout_oid: ObjectId,
    /// Checkout disposition after fetch and default-branch setup.
    pub checkout: CloneCheckout,
    /// Remote-tracking branch names that resolved to a commit (empty unless checkout is
    /// [`CloneCheckout::UnbornDefaultBranch`]).
    pub branches_with_commits: Vec<String>,
    /// Open handle to the new repository.
    pub repo: Repository,
    /// Objects and pack bytes received during the initial fetch.
    pub transfer: TransferStats,
    /// Remote branches fetched (from the fetch ref updates).
    pub branches: u64,
    /// Remote tags fetched (from the fetch ref updates).
    pub tags: u64,
}

/// Derive a destination directory from a clone URL (Git [`git_url_basename`]).
///
/// # Errors
///
/// Returns [`NoDirectoryName`] when no directory name can be guessed (same as `git clone`
/// without `<dir>`).
pub fn derive_clone_dir(url: &str) -> std::result::Result<String, NoDirectoryName> {
    git_url_basename(url, false, false)
}

/// Whether `path` is an existing empty directory (used for failed-clone cleanup policy).
#[must_use]
pub fn dest_preexisted_empty(path: &Path) -> bool {
    path.is_dir()
        && path
            .read_dir()
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
}

/// Remove a partially created clone destination.
///
/// When `keep_toplevel` is true, only contents of `path` are removed (empty dir existed before clone).
pub fn cleanup_failed_clone(path: &Path, keep_toplevel: bool) {
    if keep_toplevel {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let _ = fs::remove_dir_all(p);
            } else {
                let _ = fs::remove_file(p);
            }
        }
    } else {
        let _ = fs::remove_dir_all(path);
    }
}

/// Initialize, fetch, and configure the default branch; does not check out files.
///
/// # Errors
///
/// Returns [`CloneError`] when the destination is invalid or fetch fails.
pub fn clone(
    opts: CloneOptions,
    progress: &mut dyn Progress,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<CloneOutcome, CloneError> {
    if !crate::remote::is_valid_remote_name(&opts.remote_name) {
        return Err(CloneError::Remote(RemoteError::InvalidRemoteName(
            opts.remote_name.clone(),
        )));
    }

    let dir = opts
        .dest
        .to_str()
        .ok_or_else(|| CloneError::Library(Error::Message("non-UTF-8 destination path".into())))?;
    if opts.dest.is_dir()
        && opts
            .dest
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    {
        return Err(CloneError::DestNotEmpty(dir.to_owned()));
    }

    init_repository(
        &opts.dest,
        false,
        &opts.initial_branch,
        None,
        RefStorageFormat::Files,
    )
    .map_err(|e| {
        CloneError::Library(Error::Message(format!("could not initialize '{dir}': {e}")))
    })?;
    let git_dir = opts.dest.join(".git");
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::with_environment(opts.environment.clone())
            .with_work_tree(opts.dest.clone()),
    )
    .map_err(CloneError::Library)?;

    let origin_url = stored_clone_remote_url(&opts.url);
    set_local_config(
        &repo,
        &[(
            &format!("remote.{}.url", opts.remote_name),
            origin_url.clone(),
        )],
    )?;

    let config = ConfigSet::load(&opts.environment, Some(repo.git_dir()), true)
        .map_err(CloneError::Library)?;
    let remote = Remote::from_config(&config, &opts.remote_name)?;
    let fetch_refspec = clone_initial_fetch_refspec(&opts, &remote, &repo, http_factory)?;
    let mut remote_config_entries = vec![(
        format!("remote.{}.fetch", opts.remote_name),
        fetch_refspec.clone(),
    )];
    if matches!(opts.tags, Some(CloneTags::None)) {
        remote_config_entries.push((
            format!("remote.{}.tagOpt", opts.remote_name),
            "--no-tags".to_owned(),
        ));
    }
    set_local_config(
        &repo,
        &remote_config_entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect::<Vec<_>>(),
    )?;

    let clone_message = format!("clone: from {}", opts.url);
    let identity =
        fetch_operation_identity(&opts.environment, repo.git_dir()).map_err(CloneError::Library)?;
    let clone_reflog = CloneReflog {
        identity: identity.clone(),
        message: clone_message.clone(),
    };
    let mut fetch_opts = FetchOptions {
        refspecs: vec![fetch_refspec],
        tags: resolve_clone_tag_mode(&opts),
        initial_remote_fetch: true,
        remote_name: Some(opts.remote_name.clone()),
        clone_reflog: Some(clone_reflog),
        environment: opts.environment.clone(),
        thread_policy: opts.thread_policy,
        remote_progress: progress.remote_sideband_progress(),
        fetch_depth: opts.depth.and_then(crate::transfer::FetchDepth::depth),
        ..Default::default()
    };
    if let Ok(v) = crate::protocol::try_effective_client_protocol_version_from_config(&config) {
        fetch_opts.http_protocol_version = Some(v);
    }
    let fetch_outcome = remote.fetch(&repo, fetch_opts, progress, http_factory)?;

    let default = opts.branch.clone().unwrap_or_else(|| {
        choose_clone_default_branch(
            &repo,
            &fetch_outcome,
            &opts.url,
            &opts.remote_name,
            &opts.initial_branch,
        )
    });

    let tracking = format!("refs/remotes/{}/{default}", opts.remote_name);
    let oid = refs::resolve_ref(repo.git_dir(), &tracking).unwrap_or(ObjectId::zero());
    let branch_ref = format!("refs/heads/{default}");
    let zero = ObjectId::zero();
    if !oid.is_zero() {
        refs::write_ref(repo.git_dir(), &branch_ref, &oid).map_err(CloneError::Library)?;
        refs::append_reflog(
            repo.git_dir(),
            &branch_ref,
            &zero,
            &oid,
            &identity,
            &clone_message,
            false,
        )
        .map_err(CloneError::Library)?;
    }
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", &branch_ref).map_err(CloneError::Library)?;
    if !oid.is_zero() {
        refs::append_reflog(
            repo.git_dir(),
            "HEAD",
            &zero,
            &oid,
            &identity,
            &clone_message,
            false,
        )
        .map_err(CloneError::Library)?;
    }
    set_local_config(
        &repo,
        &[
            (
                &format!("branch.{default}.remote"),
                opts.remote_name.clone(),
            ),
            (
                &format!("branch.{default}.merge"),
                format!("refs/heads/{default}"),
            ),
        ],
    )?;

    let branches_with_commits = remote_branch_names_with_commits(&repo, &opts.remote_name);
    let checkout = classify_clone_checkout(
        oid,
        &fetch_outcome.transfer,
        fetch_outcome.branch_count(),
        &branches_with_commits,
    );

    Ok(CloneOutcome {
        url: opts.url.clone(),
        path: opts.dest.clone(),
        branch: default,
        checkout_oid: oid,
        checkout,
        branches_with_commits,
        repo: repo.with_thread_policy(opts.thread_policy),
        transfer: fetch_outcome.transfer,
        branches: fetch_outcome.branch_count(),
        tags: fetch_outcome.tag_count(),
    })
}

fn remote_branch_names_with_commits(repo: &Repository, remote_name: &str) -> Vec<String> {
    let prefix = format!("refs/remotes/{remote_name}/");
    refs::list_refs(repo.git_dir(), &prefix)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, oid)| {
            if oid.is_zero() {
                return None;
            }
            name.strip_prefix(&prefix)
                .filter(|b| *b != "HEAD")
                .map(str::to_owned)
        })
        .collect()
}

fn classify_clone_checkout(
    checkout_oid: ObjectId,
    transfer: &TransferStats,
    branch_count: u64,
    branches_with_commits: &[String],
) -> CloneCheckout {
    if !checkout_oid.is_zero() {
        return CloneCheckout::CheckedOut(checkout_oid);
    }
    if transfer.objects == 0 && branch_count == 0 {
        return CloneCheckout::EmptyRemote;
    }
    if transfer.objects > 0 || !branches_with_commits.is_empty() {
        return CloneCheckout::UnbornDefaultBranch;
    }
    CloneCheckout::EmptyRemote
}

/// Whether the clone fetches only one branch (`git clone --single-branch` semantics).
#[must_use]
pub fn clone_effective_single_branch(opts: &CloneOptions) -> bool {
    match opts.single_branch {
        Some(true) => true,
        Some(false) => false,
        None => opts.depth.is_some_and(|d| d > 0),
    }
}

fn resolve_clone_tag_mode(opts: &CloneOptions) -> TagMode {
    opts.tags.map(CloneTags::tag_mode).unwrap_or_else(|| {
        if opts.depth.is_some() {
            TagMode::Following
        } else {
            TagMode::All
        }
    })
}

/// Initial `remote.*.fetch` refspec and the refspec used for the first fetch.
fn clone_initial_fetch_refspec(
    opts: &CloneOptions,
    remote: &Remote,
    repo: &Repository,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<String, CloneError> {
    if !clone_effective_single_branch(opts) {
        return Ok(format!("+refs/heads/*:refs/remotes/{}/*", opts.remote_name));
    }
    let branch = if let Some(branch) = opts.branch.clone() {
        branch
    } else {
        advertised_default_branch_from_remote(
            remote,
            repo,
            http_factory,
            &opts.url,
            &opts.initial_branch,
        )?
    };
    Ok(format!(
        "+refs/heads/{branch}:refs/remotes/{}/{}",
        opts.remote_name, branch
    ))
}

fn advertised_default_branch_from_remote(
    remote: &Remote,
    repo: &Repository,
    http_factory: Option<&dyn HttpClientFactory>,
    url: &str,
    initial_branch: &str,
) -> std::result::Result<String, CloneError> {
    let refs = remote
        .list_refs(
            Some(repo),
            &ListRefsOptions {
                symrefs: true,
                ..Default::default()
            },
            http_factory,
        )
        .map_err(CloneError::Remote)?;
    for entry in &refs {
        if entry.name == "HEAD" {
            if let Some(target) = &entry.symref_target {
                let name = target
                    .strip_prefix("refs/heads/")
                    .unwrap_or(target.as_str());
                if !name.is_empty() {
                    return Ok(name.to_owned());
                }
            }
        }
    }
    if let Some(name) = pick_default_branch_from_remote_refs(&refs) {
        return Ok(name);
    }
    if let Some(name) = default_branch_from_local_remote(url, repo) {
        return Ok(name);
    }
    Ok(initial_branch.to_owned())
}

fn pick_default_branch_from_remote_refs(refs: &[crate::remote::RemoteRef]) -> Option<String> {
    for candidate in ["main", "master"] {
        let full = format!("refs/heads/{candidate}");
        if refs.iter().any(|r| r.name == full) {
            return Some(candidate.to_owned());
        }
    }
    refs.iter()
        .find_map(|r| r.name.strip_prefix("refs/heads/").map(str::to_owned))
}

fn stored_clone_remote_url(url: &str) -> String {
    if should_store_absolute_local_clone_url(url) {
        absolute_local_clone_source_url(Path::new(url.trim()))
    } else {
        url.to_owned()
    }
}

fn set_local_config(repo: &Repository, entries: &[(&str, String)]) -> Result<()> {
    let path = repo.git_dir().join("config");
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut config = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
    for (key, value) in entries {
        config.set(key, value)?;
    }
    config.write()?;
    Ok(())
}

fn choose_clone_default_branch(
    repo: &Repository,
    fetch_outcome: &crate::transfer::FetchOutcome,
    url: &str,
    remote_name: &str,
    initial_branch: &str,
) -> String {
    if let Some(advertised) = fetch_outcome.default_branch.as_deref() {
        let name = advertised.strip_prefix("refs/heads/").unwrap_or(advertised);
        if !name.is_empty() {
            return name.to_owned();
        }
    }

    let mut candidates: Vec<String> = Vec::new();
    let mut push = |name: Option<String>| {
        if let Some(name) = name {
            if !name.is_empty() && !candidates.iter().any(|c| c == &name) {
                candidates.push(name);
            }
        }
    };
    push(pick_default_branch(repo, remote_name));
    push(default_branch_from_local_remote(url, repo));
    push(Some(initial_branch.to_owned()));

    for name in &candidates {
        let tracking = format!("refs/remotes/{remote_name}/{name}");
        if refs::resolve_ref(repo.git_dir(), &tracking).is_ok() {
            return name.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| initial_branch.to_owned())
}

fn default_branch_from_local_remote(url: &str, repo: &Repository) -> Option<String> {
    if !url_is_local_not_ssh(url.trim()) {
        return None;
    }
    let remote_git = resolve_local_remote_git_dir(url.trim(), repo.git_dir(), repo.work_tree());
    crate::gc::remote_default_branch_local(&remote_git)
        .ok()
        .flatten()
}

fn pick_default_branch(repo: &Repository, remote_name: &str) -> Option<String> {
    for candidate in ["main", "master"] {
        if refs::resolve_ref(
            repo.git_dir(),
            &format!("refs/remotes/{remote_name}/{candidate}"),
        )
        .is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    refs::list_refs(repo.git_dir(), &format!("refs/remotes/{remote_name}/"))
        .ok()?
        .into_iter()
        .find_map(|(name, _)| {
            name.strip_prefix(&format!("refs/remotes/{remote_name}/"))
                .filter(|b| *b != "HEAD")
                .map(str::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dest_preexisted_empty_missing_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        assert!(!dest_preexisted_empty(&missing));
    }

    #[test]
    fn dest_preexisted_empty_empty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(dest_preexisted_empty(dir.path()));
    }

    #[test]
    fn dest_preexisted_empty_nonempty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("file"), b"x").expect("write");
        assert!(!dest_preexisted_empty(dir.path()));
    }

    #[test]
    fn dest_not_empty_skips_failed_clone_cleanup() {
        assert!(!CloneError::DestNotEmpty("x".into()).should_cleanup_failed_clone());
        assert!(CloneError::Library(Error::Message("x".into())).should_cleanup_failed_clone());
    }

    #[test]
    fn outcome_reports_transfer_stats() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let upstream_wt = upstream.path().join("src");
        std::fs::create_dir_all(&upstream_wt).expect("mkdir");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&upstream_wt)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(upstream_wt.join("README.md"), b"hello\n").expect("write");
        git(&["add", "README.md"]);
        git(&["commit", "-m", "init"]);
        git(&["tag", "v1"]);
        git(&["branch", "feature"]);
        std::fs::write(upstream_wt.join("extra.txt"), b"x\n").expect("write");
        git(&["add", "extra.txt"]);
        git(&["commit", "-m", "on feature"]);
        git(&["checkout", "main"]);

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(upstream_wt.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            None,
        )
        .expect("clone");

        let count_out = Command::new("git")
            .args(["count-objects", "-v"])
            .current_dir(dest.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("count-objects");
        assert!(
            count_out.status.success(),
            "{}",
            String::from_utf8_lossy(&count_out.stderr)
        );
        let count_text = String::from_utf8_lossy(&count_out.stdout);
        let mut in_pack = None::<u64>;
        for line in count_text.lines() {
            if let Some(v) = line.strip_prefix("in-pack: ") {
                in_pack = Some(v.trim().parse().expect("in-pack"));
            }
        }
        let git_objects = in_pack.expect("in-pack line");
        let git_dir = dest.path().join(".git");
        let pack_dir = git_dir.join("objects/pack");
        let pack_bytes = std::fs::read_dir(&pack_dir)
            .expect("pack dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "pack"))
            .map(|p| std::fs::metadata(&p).expect("pack stat").len())
            .sum::<u64>();
        assert_eq!(
            outcome.transfer.objects, git_objects,
            "object count must match git count-objects -v in-pack"
        );
        assert_eq!(
            outcome.transfer.bytes, pack_bytes,
            "pack bytes must match on-disk .pack file size"
        );
        assert!(outcome.branches >= 2, "expected at least main and feature");
        assert!(outcome.tags >= 1, "expected tag v1");
    }

    #[test]
    fn clone_unborn_advertised_trunk_not_populated_feature() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let bare = upstream.path().join("origin.git");
        let git = |args: &[&str], cwd: &std::path::Path| {
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "trunk",
                bare.to_str().unwrap(),
            ],
            upstream.path(),
        );
        git(&["symbolic-ref", "HEAD", "refs/heads/trunk"], &bare);
        let wt = upstream.path().join("wt");
        fs::create_dir_all(&wt).expect("mkdir");
        git(&["init", "-q"], &wt);
        git(&["config", "user.email", "t@example.com"], &wt);
        git(&["config", "user.name", "Test"], &wt);
        fs::write(wt.join("f"), b"x\n").expect("write");
        git(&["add", "f"], &wt);
        git(&["commit", "-m", "c"], &wt);
        git(&["branch", "feature"], &wt);
        git(&["push", bare.to_str().unwrap(), "feature"], &wt);

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(bare.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            None,
        )
        .expect("clone");

        assert_eq!(outcome.branch, "trunk");
        assert!(outcome.checkout_oid.is_zero());
        assert_eq!(outcome.checkout, CloneCheckout::UnbornDefaultBranch);
        assert!(
            outcome
                .branches_with_commits
                .contains(&"feature".to_owned()),
            "{:?}",
            outcome.branches_with_commits
        );
        assert!(!dest.path().join(".git/refs/heads/trunk").exists());
        assert!(!dest.path().join(".git/refs/heads/feature").exists());
        let head = fs::read_to_string(dest.path().join(".git/HEAD")).expect("HEAD");
        assert_eq!(head.trim(), "ref: refs/heads/trunk");
    }

    #[test]
    fn clone_empty_bare_remote_leaves_unborn_default_branch() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let bare = upstream.path().join("origin.git");
        let out = Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&bare)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git init");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(bare.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            None,
        )
        .expect("clone empty bare");

        assert_eq!(outcome.branch, "main");
        assert!(outcome.checkout_oid.is_zero());
        assert_eq!(outcome.checkout, CloneCheckout::EmptyRemote);
        assert!(outcome.branches_with_commits.is_empty());
        assert!(!dest.path().join(".git").join("refs/heads/main").exists());
        let head = fs::read_to_string(dest.path().join(".git/HEAD")).expect("HEAD");
        assert_eq!(head.trim(), "ref: refs/heads/main");
    }

    #[test]
    fn classify_unborn_default_when_master_advertised_but_main_has_commits() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let bare = upstream.path().join("remote.git");
        let git = |args: &[&str], cwd: &std::path::Path| {
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(
            &["init", "-q", "--bare", bare.to_str().unwrap()],
            upstream.path(),
        );

        let src = upstream.path().join("src");
        fs::create_dir_all(&src).expect("mkdir");
        git(&["init", "-q", "-b", "main"], &src);
        git(&["config", "user.email", "a@b.c"], &src);
        git(&["config", "user.name", "a"], &src);
        fs::write(src.join("f"), b"x\n").expect("write");
        git(&["add", "f"], &src);
        git(&["commit", "-m", "c1"], &src);
        git(&["remote", "add", "origin", bare.to_str().unwrap()], &src);
        git(&["push", "-u", "origin", "main"], &src);

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(bare.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            None,
        )
        .expect("clone");

        assert!(outcome.checkout_oid.is_zero());
        assert_eq!(outcome.checkout, CloneCheckout::UnbornDefaultBranch);
        assert!(
            outcome.branches_with_commits.contains(&"main".to_owned()),
            "{:?}",
            outcome.branches_with_commits
        );
        assert!(outcome.transfer.objects > 0);
    }

    #[test]
    fn derive_clone_dir_windows_style_paths() {
        assert_eq!(
            derive_clone_dir(r"..\..\origin.git").expect("name"),
            "origin"
        );
        assert_eq!(
            derive_clone_dir(r"C:\scratch\a\origin.git").expect("name"),
            "origin"
        );
    }

    #[test]
    fn clone_rejects_nonempty_destination_without_git_dir() {
        let upstream = tempfile::tempdir().expect("upstream");
        let dest = tempfile::tempdir().expect("dest");
        fs::write(dest.path().join("notes.txt"), b"keep").expect("write");
        let err = clone(
            CloneOptions::new(upstream.path().to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            None,
        )
        .expect_err("non-empty dest");
        assert!(matches!(err, CloneError::DestNotEmpty(_)));
        assert!(dest.path().join("notes.txt").is_file());
        assert!(!dest.path().join(".git").exists());
    }
}
