//! SHA-256 tree digest over repository file contents ([tree-sha256] spec).
//!
//! [tree-sha256]: https://github.com/schacon/tree-sha256
//!
//! ```rust
//! use grit_lib::tree_sha256::TreeSha256Options;
//! # fn main() {
//! let _ = TreeSha256Options::default();
//! # }
//! ```

/// Tag header name for the tree-sha256 digest (reference tool: [`HEADER`](https://github.com/schacon/tree-sha256)).
pub const TREE_SHA256_HEADER: &str = "tree-sha256";

/// Tag header naming a submodule path excluded from the digest.
pub const TREE_SHA256_EXCLUDED_HEADER: &str = "tree-sha256-excluded";

mod tag_verify;

pub use tag_verify::{
    parse_tree_sha256_tag_headers, verify_tag, SignatureVerdict, TagVerification, TagVerifyOptions,
    TreeSha256TagHeaders, TreeSha256Verdict,
};

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use crate::config::ConfigSet;
use crate::diff::submodule_embedded_git_dir;
use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::hash::{try_par_hash_with, try_par_hash_with_force, PAR_HASH_MIN_ITEMS};
use crate::objects::{
    iter_extra_headers, parse_commit, parse_tree, remove_header_lines, HashAlgo, ObjectId,
    ObjectKind, TagData, TreeEntry,
};
use crate::parallel::ParallelHashError;
use crate::parallel::ParallelOp;
use crate::repo::Repository;
use crate::submodule_gitdir::{submodule_modules_git_dir, submodule_path_config_enabled};

/// 32-byte tree-sha256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeSha256(pub [u8; 32]);

impl TreeSha256 {
    /// Digest bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex encoding (64 characters).
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

impl fmt::Display for TreeSha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for TreeSha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TreeSha256").field(&self.to_hex()).finish()
    }
}

impl FromStr for TreeSha256 {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::TreeSha256InvalidHex);
        }
        let mut out = [0_u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_nibble(chunk[0])?;
            let lo = hex_nibble(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::TreeSha256InvalidHex),
    }
}

/// Submodule exclusion and glob options for [`Repository::tree_sha256`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreeSha256Options {
    /// Glob patterns; `*` matches any byte including `/`, `?` one byte. Trailing `/` is trimmed.
    pub exclude_globs: Vec<String>,
    /// Submodule paths excluded exactly (full path from repository root).
    pub exclude_exact: Vec<String>,
}

/// Outcome of a successful tree-sha256 computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSha256Report {
    /// Final tree-sha256 digest.
    pub digest: TreeSha256,
    /// Regular file, executable, and symlink entries hashed.
    pub files: usize,
    /// Submodule entries whose contents were hashed recursively.
    pub submodules: usize,
    /// Excluded submodule paths and the gitlink commit recorded.
    pub excluded: Vec<(String, ObjectId)>,
    /// Submodule paths where a cycle was detected in the commit chain.
    pub cycles: Vec<String>,
}

/// Why a submodule could not be hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MissingSubmoduleReason {
    /// Submodule work tree or git directory is not present.
    NotCheckedOut,
    /// Gitlink commit is missing from the submodule object database.
    MissingCommit,
}

/// One submodule that blocked hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSubmodule {
    /// Full path from the superproject root.
    pub path: String,
    /// What went wrong.
    pub reason: MissingSubmoduleReason,
}

/// Compute tree-sha256 for `repo` at `oid` (peels annotated tags to commits).
///
/// # Errors
///
/// Returns [`Error::TreeSha256MissingSubmodules`] when any submodule required for
/// hashing is missing; the error lists every missing path.
pub fn tree_sha256(
    repo: &Repository,
    oid: ObjectId,
    options: &TreeSha256Options,
) -> Result<TreeSha256Report> {
    tree_sha256_with_chain(repo, oid, options, &[], "")
}

fn tree_sha256_with_chain(
    repo: &Repository,
    oid: ObjectId,
    options: &TreeSha256Options,
    parent_chain: &[ObjectId],
    path_prefix_from_root: &str,
) -> Result<TreeSha256Report> {
    let tree_oid = peel_to_tree(repo, oid)?;
    let root_commit = peel_to_commit(repo, oid)?;
    let mut commit_chain: Vec<ObjectId> = parent_chain.to_vec();
    if let Some(c) = root_commit {
        if !commit_chain.contains(&c) {
            commit_chain.push(c);
        }
    }
    let mut ctx = ComputeCtx {
        repo,
        options,
        commit_chain,
        path_prefix_from_root: path_prefix_from_root.to_owned(),
        records: Vec::new(),
        missing: Vec::new(),
        files: 0,
        submodules: 0,
        excluded: Vec::new(),
        cycles: Vec::new(),
        blob_jobs: Vec::new(),
    };
    ctx.walk_tree(tree_oid, b"")?;
    ctx.finish_blob_jobs()?;
    if !ctx.missing.is_empty() {
        return Err(Error::TreeSha256MissingSubmodules(ctx.missing));
    }
    ctx.records
        .sort_by(|a, b| record_sort_key(a).cmp(record_sort_key(b)));
    let mut concat = Vec::new();
    for rec in &ctx.records {
        concat.extend_from_slice(rec);
    }
    let digest_bytes = HashAlgo::Sha256.checksum_digest(&concat);
    let digest_arr: [u8; 32] = digest_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Message("sha256 digest length".into()))?;
    let digest = TreeSha256(digest_arr);
    Ok(TreeSha256Report {
        digest,
        files: ctx.files,
        submodules: ctx.submodules,
        excluded: ctx.excluded,
        cycles: ctx.cycles,
    })
}

fn peel_to_tree(repo: &Repository, oid: ObjectId) -> Result<ObjectId> {
    let (peeled, kind) = repo.odb().peel_tags(oid)?;
    match kind {
        ObjectKind::Tree => Ok(peeled),
        ObjectKind::Commit => {
            let obj = repo.odb().read(&peeled)?;
            Ok(parse_commit(&obj.data)?.tree)
        }
        other => Err(Error::TreeSha256InvalidRevision { found: other }),
    }
}

fn peel_to_commit(repo: &Repository, oid: ObjectId) -> Result<Option<ObjectId>> {
    repo.odb().peel_to_commit(oid)
}

fn record_sort_key(record: &[u8]) -> &[u8] {
    let Some(sp) = record.iter().position(|&b| b == b' ') else {
        return record;
    };
    let start = sp + 1;
    let end = record.iter().rposition(|&b| b == 0).unwrap_or(record.len());
    &record[start..end]
}

struct BlobJob {
    path: Vec<u8>,
    oid: ObjectId,
}

struct ComputeCtx<'a> {
    repo: &'a Repository,
    options: &'a TreeSha256Options,
    commit_chain: Vec<ObjectId>,
    /// Path from the top-level repository to this walk's root (empty at the superproject).
    path_prefix_from_root: String,
    records: Vec<Vec<u8>>,
    missing: Vec<MissingSubmodule>,
    files: usize,
    submodules: usize,
    excluded: Vec<(String, ObjectId)>,
    cycles: Vec<String>,
    blob_jobs: Vec<BlobJob>,
}

impl ComputeCtx<'_> {
    fn walk_tree(&mut self, tree_oid: ObjectId, prefix: &[u8]) -> Result<()> {
        let obj = self.repo.odb().read(&tree_oid)?;
        if obj.kind != ObjectKind::Tree {
            return Err(Error::CorruptObject(format!("expected tree {tree_oid}")));
        }
        let entries = parse_tree(&obj.data)?;
        for TreeEntry { mode, name, oid } in entries {
            if name.contains(&b'/') || name.contains(&0) {
                return Err(Error::CorruptObject("invalid tree entry name".into()));
            }
            let path = join_path_bytes(prefix, &name);
            const S_IFMT: u32 = 0o170000;
            const S_IFDIR: u32 = 0o040000;
            const S_IFREG: u32 = 0o100000;
            const S_IFLNK: u32 = 0o120000;
            const S_IFGITLINK: u32 = 0o160000;
            match mode & S_IFMT {
                S_IFDIR => {
                    let mut child_prefix = path;
                    child_prefix.push(b'/');
                    self.walk_tree(oid, &child_prefix)?;
                }
                S_IFREG | S_IFLNK => {
                    self.blob_jobs.push(BlobJob { path, oid });
                }
                S_IFGITLINK => self.handle_gitlink(&path, oid)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_gitlink(&mut self, path: &[u8], gitlink: ObjectId) -> Result<()> {
        let global_path = self.global_path_from_root(path);
        if self.is_excluded(path) {
            self.push_excluded_record(path, gitlink);
            self.excluded.push((global_path.clone(), gitlink));
            return Ok(());
        }
        if gitlink_in_commit_chain(&self.commit_chain, gitlink) {
            self.push_cycle_record(path, gitlink);
            self.cycles.push(global_path.clone());
            return Ok(());
        }
        let Some(sub_repo) = self.open_submodule_repo(path)? else {
            self.missing.push(MissingSubmodule {
                path: global_path,
                reason: MissingSubmoduleReason::NotCheckedOut,
            });
            return Ok(());
        };
        if sub_repo.odb().read(&gitlink).is_err() {
            self.missing.push(MissingSubmodule {
                path: global_path,
                reason: MissingSubmoduleReason::MissingCommit,
            });
            return Ok(());
        }
        self.commit_chain.push(gitlink);
        let sub_opts = TreeSha256Options {
            exclude_globs: self.options.exclude_globs.clone(),
            exclude_exact: self.options.exclude_exact.clone(),
        };
        match tree_sha256_with_chain(
            &sub_repo,
            gitlink,
            &sub_opts,
            &self.commit_chain,
            &global_path,
        ) {
            Ok(sub_report) => {
                self.commit_chain.pop();
                self.push_submodule_record(path, &sub_report.digest);
                self.files += sub_report.files;
                self.submodules += 1 + sub_report.submodules;
                // Submodule reports already record excluded/cycle paths from the top repo.
                self.excluded.extend(sub_report.excluded);
                self.cycles.extend(sub_report.cycles);
            }
            Err(Error::TreeSha256MissingSubmodules(mut miss)) => {
                self.commit_chain.pop();
                for m in &mut miss {
                    m.path = qualify_submodule_path(&global_path, &m.path);
                }
                self.missing.append(&mut miss);
            }
            Err(e) => {
                self.commit_chain.pop();
                return Err(e);
            }
        }
        Ok(())
    }

    fn global_path_from_root(&self, local_path: &[u8]) -> String {
        let local = path_bytes_to_string(local_path);
        if self.path_prefix_from_root.is_empty() {
            return local;
        }
        qualify_submodule_path(&self.path_prefix_from_root, &local)
    }

    fn is_excluded(&self, path: &[u8]) -> bool {
        let global = self.global_path_from_root(path);
        if self
            .options
            .exclude_exact
            .iter()
            .any(|p| p.as_bytes() == path || p == &global)
        {
            return true;
        }
        self.options
            .exclude_globs
            .iter()
            .any(|g| path_matches_glob_bytes(path, g.as_bytes()) || path_matches_glob(&global, g))
    }

    fn open_submodule_repo(&self, rel_path: &[u8]) -> Result<Option<Repository>> {
        let wt = match self.repo.work_tree() {
            Some(w) => w,
            None => return Ok(None),
        };
        let sub_wt = wt.join(path_bytes_to_os(rel_path));
        if !sub_wt.is_dir() {
            return Ok(None);
        }
        if let Some(git_dir) = submodule_embedded_git_dir(&sub_wt) {
            return Repository::open(&git_dir, &RepositoryOptions::empty().with_work_tree(sub_wt))
                .map(Some);
        }
        let cfg = self.repo.config()?;
        let rel_str = path_bytes_to_string(rel_path);
        let name = submodule_name_for_path(&cfg, &rel_str).unwrap_or(rel_str);
        let git_dir = resolve_submodule_git_dir(wt, self.repo.common_git_dir(), &cfg, &name)?;
        if !git_dir.join("HEAD").exists() {
            return Ok(None);
        }
        Repository::open(&git_dir, &RepositoryOptions::empty().with_work_tree(sub_wt)).map(Some)
    }

    fn finish_blob_jobs(&mut self) -> Result<()> {
        if self.blob_jobs.is_empty() {
            return Ok(());
        }
        let cfg = self.repo.config()?;
        // Blob SHA-256 is CPU-bound; use `pack.threads` (defaults to all CPUs), not
        // `DiffContent` (serial unless `core.preloadIndex` is enabled).
        let threads = self
            .repo
            .thread_policy()
            .try_resolve(ParallelOp::PackWrite, &cfg)
            .map(|opts| opts.parallelism().threads())
            .unwrap_or_else(|_| {
                self.repo
                    .thread_policy()
                    .resolve(ParallelOp::DiffContent, &cfg)
                    .parallelism()
                    .threads()
            });
        let odb = self.repo.odb();
        let hash_one = |job: &BlobJob| {
            let obj = odb.read(&job.oid)?;
            if !matches!(obj.kind, ObjectKind::Blob) {
                return Err(Error::CorruptObject(format!("expected blob {}", job.oid)));
            }
            Ok(hex_sha256_content(&obj.data))
        };
        let hashed: Vec<String> = if self.blob_jobs.len() >= PAR_HASH_MIN_ITEMS {
            try_par_hash_with_force(&self.blob_jobs, threads, hash_one)
        } else {
            let mut total_bytes = 0_usize;
            for job in &self.blob_jobs {
                total_bytes += odb.read_info(&job.oid)?.size as usize;
            }
            try_par_hash_with(&self.blob_jobs, threads, total_bytes.max(1), hash_one)
        }
        .map_err(|e| match e {
            ParallelHashError::Task(err) => err,
        })?;
        for (job, hex) in self.blob_jobs.iter().zip(hashed) {
            self.records.push(file_record(&hex, &job.path));
            self.files += 1;
        }
        Ok(())
    }

    fn push_submodule_record(&mut self, path: &[u8], digest: &TreeSha256) {
        let mut rec = digest.to_hex().into_bytes();
        rec.push(b' ');
        rec.extend_from_slice(path);
        if !path.ends_with(b"/") {
            rec.push(b'/');
        }
        rec.push(0);
        self.records.push(rec);
    }

    fn push_cycle_record(&mut self, path: &[u8], commit: ObjectId) {
        let mut rec = b"cycle:".to_vec();
        rec.extend_from_slice(commit.to_hex().as_bytes());
        rec.push(b' ');
        rec.extend_from_slice(path);
        if !path.ends_with(b"/") {
            rec.push(b'/');
        }
        rec.push(0);
        self.records.push(rec);
    }

    fn push_excluded_record(&mut self, path: &[u8], commit: ObjectId) {
        let mut rec = b"excluded:".to_vec();
        rec.extend_from_slice(commit.to_hex().as_bytes());
        rec.push(b' ');
        rec.extend_from_slice(path);
        if !path.ends_with(b"/") {
            rec.push(b'/');
        }
        rec.push(0);
        self.records.push(rec);
    }
}

fn join_path_bytes(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(prefix.len().saturating_add(name.len()));
    path.extend_from_slice(prefix);
    path.extend_from_slice(name);
    path
}

fn path_bytes_to_string(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn path_bytes_to_os(path: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(path).to_os_string()
    }
    #[cfg(not(unix))]
    {
        path_bytes_to_string(path).into()
    }
}

fn qualify_submodule_path(gitlink_path: &str, sub_path: &str) -> String {
    if sub_path.is_empty() {
        return gitlink_path.to_owned();
    }
    let base = gitlink_path.trim_end_matches('/');
    format!("{base}/{sub_path}")
}

fn file_record(content_hex: &str, path: &[u8]) -> Vec<u8> {
    let mut rec = Vec::with_capacity(content_hex.len() + path.len() + 2);
    rec.extend_from_slice(content_hex.as_bytes());
    rec.push(b' ');
    rec.extend_from_slice(path);
    rec.push(0);
    rec
}

fn hex_sha256_content(data: &[u8]) -> String {
    let digest = HashAlgo::Sha256.checksum_digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn resolve_submodule_git_dir(
    work_tree: &Path,
    super_git_dir: &Path,
    cfg: &ConfigSet,
    submodule_name: &str,
) -> Result<PathBuf> {
    if submodule_path_config_enabled(super_git_dir) {
        let key = format!("submodule.{submodule_name}.gitdir");
        let value = cfg.get(&key).ok_or_else(|| {
            Error::Config(format!("missing {key} (submodulePathConfig enabled)").into())
        })?;
        Ok(resolve_gitdir_value(work_tree, &value))
    } else {
        Ok(submodule_modules_git_dir(super_git_dir, submodule_name))
    }
}

fn resolve_gitdir_value(work_tree: &Path, gitdir_cfg: &str) -> PathBuf {
    let p = Path::new(gitdir_cfg.trim());
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_tree.join(p)
    }
}

fn submodule_name_for_path(cfg: &ConfigSet, path: &str) -> Option<String> {
    for e in cfg.entries() {
        let key = &e.key;
        if let Some(rest) = key.strip_prefix("submodule.") {
            if let Some(name) = rest.strip_suffix(".path") {
                if e.value.as_deref() == Some(path) {
                    return Some(name.to_owned());
                }
            }
        }
    }
    None
}

#[must_use]
pub(crate) fn gitlink_in_commit_chain(chain: &[ObjectId], gitlink: ObjectId) -> bool {
    chain.contains(&gitlink)
}

/// Whether signed tags should automatically include a tree-sha256 claim (`grit.treeSha256`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeSha256Policy {
    enabled: bool,
}

impl TreeSha256Policy {
    /// Load `grit.treeSha256` (default false).
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let enabled = config
            .get_bool("grit.treeSha256")
            .and_then(|r| r.ok())
            .unwrap_or(false);
        Self { enabled }
    }

    /// When true, every signed tag grit creates gets a tree-sha256 header.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// Tree-sha256 digest, counts, and timing from tag creation.
#[derive(Debug, Clone)]
pub struct TagTreeSha256Outcome {
    /// Headers written on the tag object.
    pub claim: TreeSha256Claim,
    /// Full hash report (files, submodules, excluded gitlinks).
    pub report: TreeSha256Report,
    /// Wall time spent computing the digest.
    pub hash_elapsed: Duration,
}

/// Digest and excluded submodule paths recorded on an annotated tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSha256Claim {
    /// Recorded tree-sha256 digest.
    pub digest: TreeSha256,
    /// Submodule paths left out of the digest (sorted when written with [`Self::apply_to_tag`]).
    ///
    /// Values must be non-empty and must not contain NUL or newline when passed to
    /// [`Self::apply_to_tag`].
    pub excluded: Vec<String>,
}

/// Failure modes when reading or validating a [`TreeSha256Claim`] from tag headers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeSha256ClaimError {
    /// More than one `tree-sha256` header appeared after `tagger`.
    #[error("duplicate tree-sha256 header in tag")]
    DuplicateDigestHeader,
    /// A `tree-sha256-excluded` path was empty or contained NUL or newline.
    #[error("invalid tree-sha256-excluded path")]
    InvalidExcludedPath,
}

impl TreeSha256Claim {
    /// Read a claim from post-`tagger` extra headers on `tag`.
    ///
    /// Returns `Ok(None)` when no `tree-sha256` header is present. Commit objects are not
    /// supported.
    ///
    /// # Errors
    ///
    /// Returns [`TreeSha256ClaimError::DuplicateDigestHeader`] or
    /// [`TreeSha256ClaimError::InvalidExcludedPath`], or [`Error::TreeSha256InvalidHex`] when the
    /// digest value is not exactly 64 ASCII hex digits (no trimming; surrounding or trailing
    /// whitespace is rejected).
    pub fn from_tag(tag: &TagData) -> Result<Option<Self>> {
        let header = TREE_SHA256_HEADER.as_bytes();
        let excluded_key = TREE_SHA256_EXCLUDED_HEADER.as_bytes();
        let mut digest: Option<TreeSha256> = None;
        let mut excluded = Vec::new();
        for (key, value) in iter_extra_headers(&tag.extra_headers) {
            if key == header {
                if digest.is_some() {
                    return Err(TreeSha256ClaimError::DuplicateDigestHeader.into());
                }
                digest = Some(tree_sha256_digest_from_tag_header_value(value)?);
            } else if key == excluded_key {
                let path = std::str::from_utf8(value)
                    .map_err(|_| TreeSha256ClaimError::InvalidExcludedPath)?;
                validate_excluded_path(path)?;
                excluded.push(path.to_owned());
            }
        }
        Ok(digest.map(|d| Self {
            digest: d,
            excluded,
        }))
    }

    /// Replace any existing tree-sha256 headers on `tag` with this claim.
    ///
    /// Excluded paths are written in sorted order, then the digest line, immediately after
    /// `tagger` and before any other extra headers, matching the reference `tree-sha256-tag`
    /// layout (inside the signed payload, before the blank line).
    ///
    /// Each entry in [`Self::excluded`] must be non-empty and must not contain NUL or newline;
    /// invalid paths are not checked here — callers must only supply paths that would round-trip
    /// through [`Self::from_tag`].
    pub fn apply_to_tag(&self, tag: &mut TagData) {
        let header = TREE_SHA256_HEADER.as_bytes();
        let excluded_key = TREE_SHA256_EXCLUDED_HEADER.as_bytes();
        let unrelated = remove_header_lines(&tag.extra_headers, |key| {
            key == header || key == excluded_key
        });
        let mut claim = Vec::new();
        let mut paths = self.excluded.clone();
        paths.sort();
        for path in paths {
            claim.extend_from_slice(TREE_SHA256_EXCLUDED_HEADER.as_bytes());
            claim.push(b' ');
            claim.extend_from_slice(path.as_bytes());
            claim.push(b'\n');
        }
        claim.extend_from_slice(TREE_SHA256_HEADER.as_bytes());
        claim.push(b' ');
        claim.extend_from_slice(self.digest.to_hex().as_bytes());
        claim.push(b'\n');
        tag.extra_headers = claim;
        tag.extra_headers.extend_from_slice(&unrelated);
    }

    /// Build a claim from a successful [`TreeSha256Report`] (excluded paths sorted).
    #[must_use]
    pub fn from_report(report: &TreeSha256Report) -> Self {
        let mut excluded: Vec<String> = report.excluded.iter().map(|(p, _)| p.clone()).collect();
        excluded.sort();
        Self {
            digest: report.digest,
            excluded,
        }
    }
}

/// Parse a `tree-sha256` tag header value: exactly 64 ASCII hex digits, no surrounding whitespace.
fn tree_sha256_digest_from_tag_header_value(value: &[u8]) -> Result<TreeSha256> {
    if value.len() != 64 || !value.iter().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::TreeSha256InvalidHex);
    }
    let text = std::str::from_utf8(value).map_err(|_| Error::TreeSha256InvalidHex)?;
    TreeSha256::from_str(text)
}

fn validate_excluded_path(path: &str) -> Result<()> {
    if path.is_empty() || path.bytes().any(|b| b == 0 || b == b'\n') {
        return Err(TreeSha256ClaimError::InvalidExcludedPath.into());
    }
    Ok(())
}

/// Returns true when `path` matches `pattern` (Git tree-sha256 glob rules).
#[must_use]
pub fn path_matches_glob(path: &str, pattern: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');
    path_matches_glob_bytes(path.as_bytes(), pattern.as_bytes())
}

/// Like [`path_matches_glob`], but `path` and `pattern` are raw bytes (`pattern` trailing `/` trimmed).
#[must_use]
pub fn path_matches_glob_bytes(path: &[u8], pattern: &[u8]) -> bool {
    glob_match(path, trim_trailing_slash_bytes(pattern))
}

fn trim_trailing_slash_bytes(mut pat: &[u8]) -> &[u8] {
    while pat.last() == Some(&b'/') {
        pat = &pat[..pat.len() - 1];
    }
    pat
}

fn glob_match(path: &[u8], pat: &[u8]) -> bool {
    glob_match_at(path, pat, 0, 0)
}

fn glob_match_at(path: &[u8], pat: &[u8], mut pi: usize, mut si: usize) -> bool {
    loop {
        if pi == pat.len() {
            return si == path.len();
        }
        match pat[pi] {
            b'*' => {
                pi += 1;
                if pi == pat.len() {
                    return true;
                }
                while si <= path.len() {
                    if glob_match_at(path, pat, pi, si) {
                        return true;
                    }
                    si += 1;
                }
                return false;
            }
            b'?' => {
                if si >= path.len() {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            c => {
                if si >= path.len() || path[si] != c {
                    return false;
                }
                pi += 1;
                si += 1;
            }
        }
    }
}

#[cfg(test)]
mod glob_tests {
    use super::{gitlink_in_commit_chain, path_matches_glob};
    use crate::objects::ObjectId;

    #[test]
    fn gitlink_cycle_helper() {
        let a: ObjectId = "0000000000000000000000000000000000000001".parse().unwrap();
        let b: ObjectId = "0000000000000000000000000000000000000002".parse().unwrap();
        assert!(gitlink_in_commit_chain(&[a, b, a], a));
        assert!(!gitlink_in_commit_chain(&[b], a));
    }

    #[test]
    fn star_matches_slash() {
        assert!(path_matches_glob("a/b/c", "a/*"));
        assert!(path_matches_glob("nested/sub/file", "nested/*"));
    }

    #[test]
    fn question_mark_one_byte() {
        assert!(path_matches_glob("abc", "a?c"));
        assert!(!path_matches_glob("ac", "a?c"));
    }

    #[test]
    fn trailing_slash_on_pattern_trimmed() {
        assert!(path_matches_glob("vendor", "vendor/"));
    }
}

#[cfg(test)]
mod claim_tests {
    use super::{
        TreeSha256, TreeSha256Claim, TreeSha256ClaimError, TREE_SHA256_EXCLUDED_HEADER,
        TREE_SHA256_HEADER,
    };
    use crate::error::Error;
    use crate::objects::{parse_tag, serialize_tag, TagData};
    use std::str::FromStr;

    fn sample_tag(extra: &[u8]) -> TagData {
        TagData {
            object: "15c242b3b6ace73d1884e431f0d814d7a0e827a1".parse().unwrap(),
            object_type: "commit".into(),
            tag: "t".into(),
            tagger: Some("T <t@example.com> 1 +0000".into()),
            extra_headers: extra.to_vec(),
            message: "msg\n".into(),
        }
    }

    #[test]
    fn claim_roundtrip_tag() {
        let digest = TreeSha256::from_str(
            "c29d81cdc535c4804153b7026e6b276b2dbab25d636341f6cab4179e2b1fd47a",
        )
        .unwrap();
        let claim = TreeSha256Claim {
            digest,
            excluded: vec!["z/mod".into(), "a/mod".into()],
        };
        let mut tag = sample_tag(b"other x\n".as_slice());
        claim.apply_to_tag(&mut tag);
        assert!(
            String::from_utf8_lossy(&tag.extra_headers).starts_with("tree-sha256-excluded a/mod"),
            "claim must follow tagger, not unrelated headers: {:?}",
            String::from_utf8_lossy(&tag.extra_headers)
        );
        assert!(
            String::from_utf8_lossy(&tag.extra_headers).contains("other x\n"),
            "unrelated headers must remain after the claim block"
        );
        let parsed = TreeSha256Claim::from_tag(&tag).unwrap().unwrap();
        assert_eq!(parsed.digest, digest);
        let mut want = claim.excluded.clone();
        want.sort();
        assert_eq!(parsed.excluded, want);
        let bytes = serialize_tag(&tag);
        let round = parse_tag(&bytes).unwrap();
        assert_eq!(TreeSha256Claim::from_tag(&round).unwrap(), Some(parsed));
    }

    #[test]
    fn claim_layout_matches_reference_tool_bytes() {
        let fixture = include_bytes!("../tests/fixtures/tree_sha256/reference_tool_tag.txt");
        let parsed = parse_tag(fixture).expect("parse fixture");
        let claim = TreeSha256Claim::from_tag(&parsed)
            .expect("from_tag")
            .expect("claim");
        let mut rebuilt = TagData {
            extra_headers: Vec::new(),
            ..parsed.clone()
        };
        claim.apply_to_tag(&mut rebuilt);
        assert_eq!(serialize_tag(&rebuilt), fixture);
    }

    #[test]
    fn apply_replaces_existing_claim() {
        let digest = TreeSha256::from_str(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        let old = TreeSha256Claim {
            digest,
            excluded: vec!["old/path".into()],
        };
        let new = TreeSha256Claim {
            digest: TreeSha256::from_str(
                "c29d81cdc535c4804153b7026e6b276b2dbab25d636341f6cab4179e2b1fd47a",
            )
            .unwrap(),
            excluded: vec![],
        };
        let mut tag = sample_tag(b"");
        old.apply_to_tag(&mut tag);
        new.apply_to_tag(&mut tag);
        let claim = TreeSha256Claim::from_tag(&tag).unwrap().unwrap();
        assert_eq!(claim, new);
        assert!(
            !String::from_utf8_lossy(&tag.extra_headers).contains("old/path"),
            "previous excluded path must be gone"
        );
    }

    #[test]
    fn duplicate_header_rejected() {
        let extra =
            b"tree-sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
tree-sha256 c29d81cdc535c4804153b7026e6b276b2dbab25d636341f6cab4179e2b1fd47a\n";
        let tag = sample_tag(extra);
        assert!(matches!(
            TreeSha256Claim::from_tag(&tag),
            Err(Error::TreeSha256Claim(
                TreeSha256ClaimError::DuplicateDigestHeader
            ))
        ));
    }

    #[test]
    fn bad_hex_rejected() {
        let extra = b"tree-sha256 not_valid_hex\n";
        let tag = sample_tag(extra);
        assert!(matches!(
            TreeSha256Claim::from_tag(&tag),
            Err(Error::TreeSha256InvalidHex)
        ));
    }

    #[test]
    fn digest_header_whitespace_rejected() {
        let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        for extra in [
            format!("tree-sha256 {hex} \n"),
            format!("tree-sha256  {hex}\n"),
            format!("tree-sha256 {hex}x\n"),
        ] {
            let tag = sample_tag(extra.as_bytes());
            assert!(
                matches!(
                    TreeSha256Claim::from_tag(&tag),
                    Err(Error::TreeSha256InvalidHex)
                ),
                "must reject {extra:?}"
            );
        }
    }

    #[test]
    fn apply_places_claim_immediately_after_tagger_byte_order() {
        let digest = TreeSha256::from_str(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        let claim = TreeSha256Claim {
            digest,
            excluded: vec!["sub/mod".into()],
        };
        let mut tag = sample_tag(b"other existing\n");
        claim.apply_to_tag(&mut tag);
        let want_prefix = "tree-sha256-excluded sub/mod\n\
tree-sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
other existing\n";
        assert_eq!(
            String::from_utf8_lossy(&tag.extra_headers),
            want_prefix,
            "serialized extra headers must not place unrelated lines before the claim"
        );
        let bytes = serialize_tag(&tag);
        let tagger_end = b"tagger T <t@example.com> 1 +0000\n";
        let pos = bytes
            .windows(tagger_end.len())
            .position(|w| w == tagger_end)
            .expect("tagger line");
        let after_tagger = &bytes[pos + tagger_end.len()..];
        assert!(
            after_tagger.starts_with(want_prefix.as_bytes()),
            "full tag bytes must place claim immediately after tagger"
        );
    }

    #[test]
    fn no_claim_returns_none() {
        let tag = sample_tag(b"change-id abc\n");
        assert_eq!(TreeSha256Claim::from_tag(&tag).unwrap(), None);
    }

    #[test]
    fn invalid_excluded_path_rejected() {
        let digest_line =
            b"tree-sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n";
        let cases: Vec<Vec<u8>> = vec![
            b"tree-sha256-excluded \n".to_vec(),
            b"tree-sha256-excluded bad\n more\n".to_vec(),
            b"tree-sha256-excluded nul\0here\n".to_vec(),
        ];
        for extra in &cases {
            let mut blob = extra.clone();
            blob.extend_from_slice(digest_line);
            let tag = sample_tag(&blob);
            assert!(matches!(
                TreeSha256Claim::from_tag(&tag),
                Err(Error::TreeSha256Claim(
                    TreeSha256ClaimError::InvalidExcludedPath
                ))
            ));
        }
    }

    #[test]
    fn header_constants_match_reference_tool() {
        assert_eq!(TREE_SHA256_HEADER, "tree-sha256");
        assert_eq!(TREE_SHA256_EXCLUDED_HEADER, "tree-sha256-excluded");
    }
}
