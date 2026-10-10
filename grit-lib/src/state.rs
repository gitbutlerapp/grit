//! Repository state machine — HEAD resolution, branch status, and
//! in-progress operation detection.
//!
//! # Overview
//!
//! Git repositories can be in various states beyond just "clean":
//! merging, rebasing, cherry-picking, reverting, bisecting, etc.
//! This module detects those states by checking for sentinel files
//! (e.g. `MERGE_HEAD`, `rebase-merge/`) in the `.git` directory.
//!
//! It also resolves `HEAD` to determine the current branch and commit,
//! and provides working tree / index diff summaries used by `status`,
//! `commit`, and other porcelain commands.

use std::fs;
use std::path::Path;

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::reflog;

/// The current state of HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// HEAD points to a branch via a symbolic ref (e.g. `ref: refs/heads/main`).
    Branch {
        /// The full ref name (e.g. `refs/heads/main`).
        refname: String,
        /// The short branch name (e.g. `main`).
        short_name: String,
        /// The commit OID that the branch points to, or `None` if the
        /// branch is unborn (no commits yet).
        oid: Option<ObjectId>,
    },
    /// HEAD is detached — pointing directly at a commit.
    Detached {
        /// The commit OID.
        oid: ObjectId,
    },
    /// HEAD is in an invalid or unreadable state.
    Invalid,
}

impl HeadState {
    /// Return the commit OID if HEAD resolves to one.
    #[must_use]
    pub fn oid(&self) -> Option<&ObjectId> {
        match self {
            Self::Branch { oid, .. } => oid.as_ref(),
            Self::Detached { oid } => Some(oid),
            Self::Invalid => None,
        }
    }

    /// Return the branch name if HEAD is on a branch.
    #[must_use]
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::Branch { short_name, .. } => Some(short_name),
            _ => None,
        }
    }

    /// Whether HEAD is on an unborn branch (no commits yet).
    #[must_use]
    pub fn is_unborn(&self) -> bool {
        matches!(self, Self::Branch { oid: None, .. })
    }

    /// Whether HEAD is detached.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        matches!(self, Self::Detached { .. })
    }
}

/// An in-progress operation that the repository is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgressOperation {
    /// A merge is in progress (`MERGE_HEAD` exists).
    Merge,
    /// An interactive rebase is in progress (`rebase-merge/` exists).
    RebaseInteractive,
    /// A non-interactive rebase is in progress (`rebase-apply/` exists).
    Rebase,
    /// A cherry-pick is in progress (`CHERRY_PICK_HEAD` exists).
    CherryPick,
    /// A revert is in progress (`REVERT_HEAD` exists).
    Revert,
    /// A bisect is in progress (`BISECT_LOG` exists).
    Bisect,
    /// An `am` (apply mailbox) is in progress (`rebase-apply/applying` exists).
    Am,
}

impl InProgressOperation {
    /// Human-readable description of the operation.
    #[must_use]
    pub fn description(&self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::RebaseInteractive => "interactive rebase",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
            Self::Am => "am",
        }
    }

    /// Hint text for how to continue or abort.
    #[must_use]
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Merge => "fix conflicts and run \"git commit\"\n  (use \"git merge --abort\" to abort the merge)",
            Self::RebaseInteractive => "fix conflicts and then run \"git rebase --continue\"\n  (use \"git rebase --abort\" to abort the rebase)",
            Self::Rebase => "fix conflicts and then run \"git rebase --continue\"\n  (use \"git rebase --abort\" to abort the rebase)",
            Self::CherryPick => "fix conflicts and run \"git cherry-pick --continue\"\n  (use \"git cherry-pick --abort\" to abort the cherry-pick)",
            Self::Revert => "fix conflicts and run \"git revert --continue\"\n  (use \"git revert --abort\" to abort the revert)",
            Self::Bisect => "use \"git bisect reset\" to get back to the original branch",
            Self::Am => "fix conflicts and then run \"git am --continue\"\n  (use \"git am --abort\" to abort the am)",
        }
    }
}

/// Full snapshot of a repository's state.
///
/// This is the information that porcelain commands like `status` need to
/// display the repository's current situation.
#[derive(Debug, Clone)]
pub struct RepoState {
    /// Current HEAD state.
    pub head: HeadState,
    /// In-progress operations (there can be multiple, e.g. rebase + merge).
    pub in_progress: Vec<InProgressOperation>,
    /// Whether the repository is bare.
    pub is_bare: bool,
}

/// Resolve HEAD from the given git directory.
///
/// Reads `HEAD`, follows symbolic refs, and resolves the final OID.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Errors
///
/// Returns [`Error::Io`] if files cannot be read.
pub fn resolve_head(git_dir: &Path) -> Result<HeadState> {
    let head_path = git_dir.join("HEAD");
    let content = match fs::read_link(&head_path) {
        Ok(link_target) => {
            let rendered = link_target.to_string_lossy();
            if link_target.is_absolute() || rendered.starts_with("refs/") {
                format!("ref: {rendered}")
            } else {
                fs::read_to_string(&head_path).map_err(Error::Io)?
            }
        }
        Err(_) => match fs::read_to_string(&head_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HeadState::Invalid),
            Err(e) => return Err(Error::Io(e)),
        },
    };

    let trimmed = content.trim();

    if let Some(refname) = trimmed.strip_prefix("ref: ") {
        let refname = if refname == "refs/heads/.invalid" {
            match crate::refs::read_ref_file(&git_dir.join("refs").join("heads")) {
                Ok(crate::refs::Ref::Symbolic(target)) => target,
                _ => refname.to_owned(),
            }
        } else {
            refname.to_owned()
        };
        if check_refname_format(&refname, &RefNameOptions::default()).is_err() {
            return Ok(HeadState::Invalid);
        }
        let short_name = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(&refname)
            .to_owned();

        // Resolve the branch tip via the shared refs backend (worktrees, packed-refs).
        // Missing `refs/heads/*` => unborn branch (`None`). A symref to a non-branch
        // target that cannot be resolved (e.g. HEAD -> `.broken`) is invalid, matching
        // Git `refs_resolve_ref_unsafe` returning no target for `worktree list`.
        let oid = match crate::refs::resolve_ref(git_dir, &refname) {
            Ok(oid) => Some(oid),
            Err(Error::InvalidRef(msg)) if msg.starts_with("ref not found:") => {
                if refname.starts_with("refs/heads/") {
                    None
                } else {
                    return Ok(HeadState::Invalid);
                }
            }
            Err(e) => return Err(e),
        };

        Ok(HeadState::Branch {
            refname,
            short_name,
            oid,
        })
    } else {
        // Detached HEAD — should be a hex OID
        match ObjectId::from_hex(trimmed) {
            Ok(oid) => Ok(HeadState::Detached { oid }),
            Err(_) => Ok(HeadState::Invalid),
        }
    }
}

/// Detect in-progress operations by checking for sentinel files.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Returns
///
/// A list of detected in-progress operations.
pub fn detect_in_progress(git_dir: &Path) -> Vec<InProgressOperation> {
    let mut ops = Vec::new();

    if git_dir.join("MERGE_HEAD").exists() {
        ops.push(InProgressOperation::Merge);
    }

    // Interactive rebase: rebase-merge/ directory
    let rebase_merge = git_dir.join("rebase-merge");
    if rebase_merge.is_dir() {
        if rebase_merge.join("interactive").exists() {
            ops.push(InProgressOperation::RebaseInteractive);
        } else {
            ops.push(InProgressOperation::Rebase);
        }
    }

    // Non-interactive rebase or am: rebase-apply/ directory
    let rebase_apply = git_dir.join("rebase-apply");
    if rebase_apply.is_dir() {
        if rebase_apply.join("applying").exists() {
            ops.push(InProgressOperation::Am);
        } else {
            ops.push(InProgressOperation::Rebase);
        }
    }

    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        ops.push(InProgressOperation::CherryPick);
    }

    if git_dir.join("REVERT_HEAD").exists() {
        ops.push(InProgressOperation::Revert);
    }

    let bisect_log = crate::refs::common_dir(git_dir)
        .unwrap_or_else(|| git_dir.to_path_buf())
        .join("BISECT_LOG");
    if bisect_log.exists() {
        ops.push(InProgressOperation::Bisect);
    }

    ops
}

/// Snapshot of repository state used by `git status` long-format output (`wt-status.c`).
///
/// This mirrors Git's `struct wt_status_state` closely enough for advice lines and
/// branch headers (merge, rebase, cherry-pick, revert, bisect, am, detached HEAD).
#[derive(Debug, Clone, Default)]
pub struct WtStatusState {
    /// `MERGE_HEAD` exists (merge or merge+rebase).
    pub merge_in_progress: bool,
    /// `.git/rebase-merge/` exists and `interactive` is present.
    pub rebase_interactive_in_progress: bool,
    /// Rebase without interactive marker (`rebase-merge` non-interactive or `rebase-apply`).
    pub rebase_in_progress: bool,
    /// Display string for the branch being rebased (from `head-name`, may be absent).
    pub rebase_branch: Option<String>,
    /// Display string for the rebase onto commit (from `onto`, abbreviated OID or name).
    pub rebase_onto: Option<String>,
    /// `rebase-apply/applying` exists.
    pub am_in_progress: bool,
    /// Empty patch in `am` session (`rebase-apply/patch` has size 0).
    pub am_empty_patch: bool,
    /// `CHERRY_PICK_HEAD` or sequencer pick without head.
    pub cherry_pick_in_progress: bool,
    /// `None` means "in progress" without a specific commit (null OID / sequencer-only).
    pub cherry_pick_head_oid: Option<ObjectId>,
    /// `REVERT_HEAD` or sequencer revert without head.
    pub revert_in_progress: bool,
    pub revert_head_oid: Option<ObjectId>,
    /// `BISECT_LOG` exists (checked under common dir).
    pub bisect_in_progress: bool,
    pub bisecting_from: Option<String>,
    /// Detached HEAD: human label (`wt_status_get_detached_from`).
    pub detached_from: Option<String>,
    /// True when `HEAD` OID equals the detached tip OID.
    pub detached_at: bool,
}

fn abbrev_oid(oid: &ObjectId) -> String {
    oid.to_hex()[..7].to_string()
}

fn read_trimmed_line(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let mut line = s.lines().next()?.to_string();
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

/// Read a single-line ref/OID file like Git `get_branch()` in `wt-status.c`.
fn get_branch_display(git_dir: &Path, rel: &str) -> Option<String> {
    let path = git_dir.join(rel);
    let mut sb = read_trimmed_line(&path)?;
    if let Some(branch_name) = sb.strip_prefix("refs/heads/") {
        sb = branch_name.to_string();
    } else if sb.starts_with("refs/") {
        // keep full ref for remotes etc.
    } else if ObjectId::from_hex(&sb).is_ok() {
        let oid = ObjectId::from_hex(&sb).ok()?;
        sb = abbrev_oid(&oid);
    } else if sb == "detached HEAD" {
        return None;
    }
    Some(sb)
}

fn strip_ref_for_display(full: &str) -> String {
    if let Some(s) = full.strip_prefix("refs/tags/") {
        return s.to_string();
    }
    if let Some(s) = full.strip_prefix("refs/remotes/") {
        return s.to_string();
    }
    if let Some(s) = full.strip_prefix("refs/heads/") {
        return s.to_string();
    }
    full.to_string()
}

fn ref_name_exists(git_dir: &Path, refname: &str) -> bool {
    git_dir.join(refname).exists()
        || crate::refs::packed_refs_entry_exists(git_dir, refname).unwrap_or(false)
}

fn dwim_detach_label(git_dir: &Path, target: &str, noid: ObjectId) -> String {
    if target == "HEAD" {
        return abbrev_oid(&noid);
    }
    if target.starts_with("refs/") {
        if let Ok(oid) = crate::refs::resolve_ref(git_dir, target) {
            if oid == noid {
                return strip_ref_for_display(target);
            }
        }
    }
    for candidate in [
        format!("refs/heads/{target}"),
        format!("refs/tags/{target}"),
        format!("refs/remotes/{target}"),
    ] {
        if candidate.starts_with("refs/tags/") && ref_name_exists(git_dir, &candidate) {
            return strip_ref_for_display(&candidate);
        }
        if let Ok(oid) = crate::refs::resolve_ref(git_dir, &candidate) {
            if oid == noid {
                return strip_ref_for_display(&candidate);
            }
        }
    }
    if ObjectId::is_hex_len(target.len()) {
        if let Ok(oid) = ObjectId::from_hex(target) {
            if oid == noid {
                return abbrev_oid(&noid);
            }
        }
    }
    // `checkout … to <abbrev>` records the object name from the user's input; show that
    // abbreviation (Git does not substitute a tag name here — see t3203 detached HEAD).
    if !target.is_empty()
        && target.chars().all(|c| c.is_ascii_hexdigit())
        && target.len() <= noid.algo().hex_len()
        && noid.to_hex().starts_with(target)
    {
        return target.to_owned();
    }
    abbrev_oid(&noid)
}

fn wt_status_get_detached_from(git_dir: &Path, head_oid: ObjectId) -> Option<(String, bool)> {
    let entries = reflog::read_reflog(git_dir, "HEAD").ok()?;
    for entry in entries.iter().rev() {
        let msg = entry.message.trim();
        let Some(rest) = msg.strip_prefix("checkout: moving from ") else {
            continue;
        };
        let Some(idx) = rest.rfind(" to ") else {
            continue;
        };
        let target = rest[idx + 4..].trim();
        let noid = entry.new_oid;
        let label = dwim_detach_label(git_dir, target, noid);
        let detached_at = head_oid == noid;
        return Some((label, detached_at));
    }
    None
}

fn wt_status_check_rebase(git_dir: &Path, state: &mut WtStatusState) -> bool {
    let apply = git_dir.join("rebase-apply");
    if apply.is_dir() {
        if apply.join("applying").exists() {
            state.am_in_progress = true;
            let patch = apply.join("patch");
            if let Ok(meta) = patch.metadata() {
                if meta.len() == 0 {
                    state.am_empty_patch = true;
                }
            }
        } else {
            state.rebase_in_progress = true;
            state.rebase_branch = get_branch_display(git_dir, "rebase-apply/head-name");
            state.rebase_onto = get_branch_display(git_dir, "rebase-apply/onto");
        }
        return true;
    }
    let merge = git_dir.join("rebase-merge");
    if merge.is_dir() {
        if merge.join("interactive").exists() {
            state.rebase_interactive_in_progress = true;
        } else {
            state.rebase_in_progress = true;
        }
        state.rebase_branch = get_branch_display(git_dir, "rebase-merge/head-name");
        state.rebase_onto = get_branch_display(git_dir, "rebase-merge/onto");
        return true;
    }
    false
}

fn sequencer_first_replay(git_dir: &Path) -> Option<bool> {
    let path = git_dir.join("sequencer").join("todo");
    if !path.is_file() {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut parts = t.split_whitespace();
        let cmd = parts.next()?;
        return match cmd {
            "pick" | "p" => Some(true),
            "revert" | "r" => Some(false),
            _ => None,
        };
    }
    None
}

/// Fill [`WtStatusState`] the same way Git `wt_status_get_state` does (without sparse checkout %).
///
/// `get_detached_from` matches Git's third parameter: when true and `head` is detached, populate
/// `detached_from` / `detached_at` from the `HEAD` reflog.
pub fn wt_status_get_state(
    git_dir: &Path,
    head: &HeadState,
    get_detached_from: bool,
) -> Result<WtStatusState> {
    let mut state = WtStatusState::default();

    if git_dir.join("MERGE_HEAD").exists() {
        wt_status_check_rebase(git_dir, &mut state);
        state.merge_in_progress = true;
    } else if wt_status_check_rebase(git_dir, &mut state) {
        // rebase/am state already filled
    } else if let Some(oid) = read_cherry_pick_head(git_dir)? {
        state.cherry_pick_in_progress = true;
        state.cherry_pick_head_oid = Some(oid);
    }

    let bisect_base = crate::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    if bisect_base.join("BISECT_LOG").exists() {
        state.bisect_in_progress = true;
        state.bisecting_from = get_branch_display(&bisect_base, "BISECT_START");
    }

    if let Some(oid) = read_revert_head(git_dir)? {
        state.revert_in_progress = true;
        state.revert_head_oid = Some(oid);
    }

    if let Some(is_pick) = sequencer_first_replay(git_dir) {
        if is_pick && !state.cherry_pick_in_progress {
            state.cherry_pick_in_progress = true;
            state.cherry_pick_head_oid = None;
        } else if !is_pick && !state.revert_in_progress {
            state.revert_in_progress = true;
            state.revert_head_oid = None;
        }
    }

    if get_detached_from {
        if let HeadState::Detached { oid } = head {
            if let Some((label, at)) = wt_status_get_detached_from(git_dir, *oid) {
                state.detached_from = Some(label);
                state.detached_at = at;
            }
        }
    }

    Ok(state)
}

/// Whether a split commit is in progress during interactive rebase (`wt-status.c` `split_commit_in_progress`).
pub fn split_commit_in_progress(git_dir: &Path, head: &HeadState) -> bool {
    let HeadState::Detached { oid: head_oid } = head else {
        return false;
    };
    let Some(amend_line) = read_trimmed_line(&git_dir.join("rebase-merge/amend")) else {
        return false;
    };
    let Some(orig_line) = read_trimmed_line(&git_dir.join("rebase-merge/orig-head")) else {
        return false;
    };
    let Ok(amend_oid) = ObjectId::from_hex(amend_line.trim()) else {
        return false;
    };
    let Ok(orig_head_oid) = ObjectId::from_hex(orig_line.trim()) else {
        return false;
    };
    if amend_line == orig_line {
        head_oid != &amend_oid
    } else if let Ok(Some(cur_orig)) = read_orig_head(git_dir) {
        cur_orig != orig_head_oid
    } else {
        false
    }
}

/// Build a complete [`RepoState`] snapshot for a repository.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
/// - `is_bare` — whether this is a bare repository.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
pub fn repo_state(git_dir: &Path, is_bare: bool) -> Result<RepoState> {
    let head = resolve_head(git_dir)?;
    let in_progress = detect_in_progress(git_dir);

    Ok(RepoState {
        head,
        in_progress,
        is_bare,
    })
}

/// Read the MERGE_HEAD file and return the OIDs listed.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Returns
///
/// A vector of merge parent OIDs, or empty if not in a merge.
pub fn read_merge_heads(git_dir: &Path) -> Result<Vec<ObjectId>> {
    let path = git_dir.join("MERGE_HEAD");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };

    let mut oids = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            oids.push(ObjectId::from_hex(trimmed)?);
        }
    }
    Ok(oids)
}

/// Read the MERGE_MSG file.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Returns
///
/// The merge message text, or `None` if not in a merge.
pub fn read_merge_msg(git_dir: &Path) -> Result<Option<String>> {
    let path = git_dir.join("MERGE_MSG");
    match fs::read_to_string(&path) {
        Ok(c) => Ok(Some(c)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Remove merge-in-progress sentinel files after a successful merge commit.
///
/// Matches the files Git drops when a merge concludes with `git commit` (not `--abort`).
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Errors
///
/// Returns [`Error::Io`] if a present sentinel file cannot be removed.
pub fn clear_merge_state(git_dir: &Path) -> Result<()> {
    const MERGE_SENTINELS: &[&str] = &[
        "MERGE_HEAD",
        "MERGE_MSG",
        "MERGE_MODE",
        "MERGE_LOG",
        "AUTO_MERGE",
        "SQUASH_MSG",
    ];
    for name in MERGE_SENTINELS {
        let path = git_dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(Error::Io)?;
        }
    }
    Ok(())
}

/// Read CHERRY_PICK_HEAD when it contains a valid 40-hex OID; `None` if missing, empty, or invalid
/// (Git ignores malformed `CHERRY_PICK_HEAD` for the "commit $abbrev" line; sequencer still applies).
pub fn read_cherry_pick_head(git_dir: &Path) -> Result<Option<ObjectId>> {
    read_oid_head_file_optional(&git_dir.join("CHERRY_PICK_HEAD"))
}

/// Read REVERT_HEAD when it contains a valid OID; `None` if missing, empty, or invalid.
pub fn read_revert_head(git_dir: &Path) -> Result<Option<ObjectId>> {
    read_oid_head_file_optional(&git_dir.join("REVERT_HEAD"))
}

fn read_oid_head_file_optional(path: &Path) -> Result<Option<ObjectId>> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(ObjectId::from_hex(trimmed).ok())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Read ORIG_HEAD.
pub fn read_orig_head(git_dir: &Path) -> Result<Option<ObjectId>> {
    read_single_oid_file(&git_dir.join("ORIG_HEAD"))
}

/// Read a file that contains a single OID on its first line.
fn read_single_oid_file(path: &Path) -> Result<Option<ObjectId>> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(ObjectId::from_hex(trimmed)?))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Check upstream (tracking) information for the current branch.
///
/// Returns `(ahead, behind)` counts relative to the tracking branch.
/// This requires commit walking and is deferred for now.
///
/// # Parameters
///
/// - `_git_dir` — path to the `.git` directory.
/// - `_branch` — the local branch name.
///
/// # Returns
///
/// `None` if no upstream is configured.
pub fn upstream_tracking(_git_dir: &Path, _branch: &str) -> Result<Option<(usize, usize)>> {
    // TODO: Implement ahead/behind counting once config + rev-list integration is ready.
    Ok(None)
}
