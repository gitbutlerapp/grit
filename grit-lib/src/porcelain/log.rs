//! `git log` ref decoration as structured data.
//!
//! `git log --decorate` / the `%d` and `%(decorate:…)` pretty placeholders attach
//! the refs that point at each commit (branches, tags, remote-tracking branches,
//! the stash, replace refs, and `HEAD`). This module computes that attachment as a
//! **colour-free [`DecorationMap`]** — for every commit hex, the ordered list of
//! [`DecorationItem`]s pointing at it — exactly the way Git's `log-tree.c` builds
//! its decoration list.
//!
//! Following the [library/CLI contract](crate::porcelain), the library produces
//! only the structured model: which ref decorates which commit, its display name,
//! and its [`DecorationKind`]. The `grit` binary owns everything presentational —
//! mapping a `DecorationKind` to a `color.decorate.*` sequence, folding
//! `HEAD -> branch`, the parentheses/separators of `%d`, and reading the clap args
//! that build the [`DecorationFilter`]. The bulk of `grit log` (rev-walk, pretty
//! formatting, the pager, graph drawing) still lives in
//! `grit/src/commands/log.rs`; this module is the first colour-free slice of it.

use std::collections::{HashMap, HashSet};

use crate::config::{parse_bool, ConfigSet};
use crate::error::Result;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

/// Decoration category for `git log --decorate` colouring (`color.decorate.*`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DecorationKind {
    /// Local branch (`refs/heads/*`).
    Branch,
    /// Remote-tracking branch (`refs/remotes/*`).
    RemoteBranch,
    /// Annotated or lightweight tag (`refs/tags/*`).
    Tag,
    /// Stash ref (`refs/stash`).
    Stash,
    /// Synthetic `HEAD` label when decorating the current tip.
    Head,
    /// Replace/graft decoration for rewritten history.
    Grafted,
    /// A ref outside the known namespaces (e.g. `refs/hidden/*`, `refs/prefetch/*`).
    /// Git renders these with `DECORATION_NONE` (reset color) and the full refname.
    /// Only decorated when an explicit `--decorate-refs[-exclude]` / `log.excludeDecoration`
    /// filter is active (or `--clear-decorations`), matching `set_default_decoration_filter`.
    Other,
}

/// One ref (or synthetic label) attached to a commit for `--decorate` / `%d`.
#[derive(Clone, Debug)]
pub struct DecorationItem {
    /// Full ref name when this came from a real ref (used for `HEAD -> branch` folding).
    pub refname: Option<String>,
    /// Short or formatted label shown inside parentheses in `%d` output.
    pub display: String,
    /// Category used by the CLI to pick `color.decorate.*` sequences.
    pub kind: DecorationKind,
}

/// Per-commit-hex decoration lists, keyed by 40-char commit hex.
pub type DecorationMap = HashMap<String, Vec<DecorationItem>>;

/// One ref label attached to a commit for structured log/show JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRefDecoration {
    /// Display name (short branch/tag label or full ref when `--decorate=full`).
    pub name: String,
    /// Decoration category (`branch`, `tag`, `HEAD`, …).
    pub kind: DecorationKind,
}

/// Commit metadata loaded once per oid for porcelain JSON (`log`, `show`, `revs`, …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedCommit {
    /// Commit object id.
    pub oid: ObjectId,
    /// Parent commit ids (empty for root commits).
    pub parents: Vec<ObjectId>,
    /// Raw author identity line.
    pub author: String,
    /// Raw committer identity line.
    pub committer: String,
    /// Full commit message body (headers excluded).
    pub message: String,
    /// Ref decorations pointing at this commit.
    pub refs: Vec<CommitRefDecoration>,
}

/// The `--decorate-refs` / `--decorate-refs-exclude` / `log.excludeDecoration`
/// ref filter. The CLI builds this from clap args + config (`build_decoration_filter`);
/// the library only applies it.
#[derive(Default, Clone)]
pub struct DecorationFilter {
    /// Ref patterns from `--decorate-refs` (must match to decorate).
    pub include: Vec<String>,
    /// Ref patterns from `--decorate-refs-exclude`.
    pub exclude: Vec<String>,
    /// Excludes from `log.excludeDecoration` config (same effect as `exclude`,
    /// but a matching `--decorate-refs` include overrides them, matching Git).
    pub exclude_config: Vec<String>,
}

impl DecorationFilter {
    /// True when no include/exclude patterns are configured.
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty() && self.exclude_config.is_empty()
    }

    /// Mirror Git's `ref_filter_match` (log-tree.c). Returns true if `refname`
    /// (a full ref like `refs/heads/foo`) should be decorated.
    pub fn matches(&self, refname: &str) -> bool {
        // `--decorate-refs-exclude` (command line) is checked first.
        for pat in &self.exclude {
            if decoration_pattern_matches(pat, refname) {
                return false;
            }
        }
        // An explicit `--decorate-refs` include is decisive and overrides the
        // config-based `log.excludeDecoration` exclusions (matching Git's
        // `ref_filter_match` ordering).
        if !self.include.is_empty() {
            for pat in &self.include {
                if decoration_pattern_matches(pat, refname) {
                    return true;
                }
            }
            return false;
        }
        // `log.excludeDecoration` only applies when there is no include list.
        for pat in &self.exclude_config {
            if decoration_pattern_matches(pat, refname) {
                return false;
            }
        }
        true
    }
}

/// Normalize a `--decorate-refs[-exclude]` / `log.excludeDecoration` pattern the
/// way Git's `normalize_glob_ref` (refs.c) does: prepend `refs/` unless it already
/// starts with `refs/` or is exactly `HEAD`, then strip a single trailing `/`.
pub fn normalize_glob_ref(pattern: &str) -> String {
    let mut out = String::new();
    if !pattern.starts_with("refs/") && pattern != "HEAD" {
        out.push_str("refs/");
    }
    out.push_str(pattern);
    if out.ends_with('/') {
        out.pop();
    }
    out
}

/// Match a single (already-normalized) `--decorate-refs[-exclude]` pattern against
/// a full refname, mirroring Git's `match_ref_pattern` (log-tree.c): glob patterns
/// use `wildmatch`; non-glob patterns match as a prefix at a component boundary.
fn decoration_pattern_matches(pattern: &str, refname: &str) -> bool {
    let has_glob = pattern.bytes().any(|b| matches!(b, b'*' | b'?' | b'['));
    if has_glob {
        crate::wildmatch::wildmatch(pattern.as_bytes(), refname.as_bytes(), 0)
    } else {
        // `skip_prefix(refname, pattern, &rest) && (!*rest || *rest == '/')`.
        if let Some(rest) = refname.strip_prefix(pattern) {
            rest.is_empty() || rest.starts_with('/')
        } else {
            false
        }
    }
}

fn replace_ref_base() -> String {
    let mut base = crate::environment::Environment::empty()
        .var("GIT_REPLACE_REF_BASE")
        .unwrap_or_else(|| "refs/replace/".to_owned());
    if !base.ends_with('/') {
        base.push('/');
    }
    base
}

fn prepend_decoration(items: &mut Vec<DecorationItem>, item: DecorationItem) {
    items.insert(0, item);
}

/// Collect ref decorations from the repository (heads, tags, remotes, stash, replace refs, HEAD).
///
/// Order matches Git's `refs_for_each_ref` walk: each ref is **prepended** in ascending ref-name
/// order, so the final per-commit list matches upstream (e.g. `tag: A1` before `other/main` before
/// `other/HEAD`).
pub fn collect_decorations(repo: &Repository, full: bool) -> Result<DecorationMap> {
    collect_decorations_inner(repo, full, false, &DecorationFilter::default(), None)
}

/// Like [`collect_decorations`], but only records decorations for the given commits.
///
/// Refs are still scanned (Git decorates by walking refs), but map entries are
/// created only for commits in `commits`, which avoids large maps when listing a
/// bounded `-n` page on repos with deep history.
pub fn collect_decorations_for_commits(
    repo: &Repository,
    full: bool,
    commits: &[ObjectId],
) -> Result<DecorationMap> {
    if commits.is_empty() {
        return Ok(HashMap::new());
    }
    let targets: HashSet<String> = commits.iter().map(ObjectId::to_hex).collect();
    collect_decorations_inner(
        repo,
        full,
        false,
        &DecorationFilter::default(),
        Some(&targets),
    )
}

/// Load commit metadata for `oids`, attaching decorations from a pre-built [`DecorationMap`].
///
/// Reads each commit object at most once. Call [`collect_decorations`] once per command
/// and reuse the map across all rows.
///
/// # Errors
///
/// Returns ODB and commit-parse errors when an oid is missing or corrupt.
pub fn load_commits_with_decorations(
    repo: &Repository,
    oids: &[ObjectId],
    decorations: &DecorationMap,
) -> Result<Vec<LoadedCommit>> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let odb = repo.odb();
    let config = repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(ConfigSet::new()));
    let par_opts = repo
        .thread_policy()
        .resolve(crate::parallel::ParallelOp::IndexLoad, &config);
    let threads = par_opts.threads;
    let thread_count = threads.get();
    let est_bytes = oids.len().saturating_mul(512);
    let load_one = |oid: &ObjectId| -> Result<LoadedCommit> {
        let obj = odb.read(oid)?;
        let data = parse_commit(&obj.data)?;
        let hex = oid.to_hex();
        let refs = decorations
            .get(&hex)
            .map(|items| {
                items
                    .iter()
                    .map(|item| CommitRefDecoration {
                        name: item.display.clone(),
                        kind: item.kind,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(LoadedCommit {
            oid: *oid,
            parents: data.parents,
            author: data.author,
            committer: data.committer,
            message: data.message,
            refs,
        })
    };
    if oids.len() >= crate::hash::PAR_HASH_MIN_ITEMS
        && crate::hash::parallel_hash_worthwhile(oids.len(), est_bytes, thread_count)
    {
        return crate::hash::try_par_hash_with(oids, threads, est_bytes, load_one).map_err(|e| {
            match e {
                crate::hash::ParallelHashError::Task(err) => err,
            }
        });
    }
    oids.iter().map(load_one).collect()
}

fn prepend_if_target(
    map: &mut DecorationMap,
    commit_hex: String,
    targets: Option<&HashSet<String>>,
    item: DecorationItem,
) {
    if targets.is_some_and(|set| !set.contains(&commit_hex)) {
        return;
    }
    prepend_decoration(map.entry(commit_hex).or_default(), item);
}

/// Like [`collect_decorations`], but `clear_decorations` removes the default ref
/// filter so refs that are normally hidden (e.g. `refs/notes/*`) also get
/// decorated (matches `git log --clear-decorations`).
///
/// When `commit_targets` is [`Some`], only commits whose hex appears in the set
/// receive map entries (refs are still scanned).
pub fn collect_decorations_inner(
    repo: &Repository,
    full: bool,
    clear_decorations: bool,
    filter: &DecorationFilter,
    commit_targets: Option<&HashSet<String>>,
) -> Result<DecorationMap> {
    let mut map: DecorationMap = HashMap::new();
    let git_dir = &repo.git_dir();
    let odb = &repo.odb();

    let head = resolve_head(git_dir)?;
    let hide_remote_update_noise = repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(ConfigSet::new()))
        .get("grit.submoduleUpdateRemoteDecorations")
        .as_deref()
        .and_then(|value| parse_bool(value).ok())
        .unwrap_or(false);
    let rep_base = replace_ref_base();

    let mut all_refs = crate::refs::list_refs(git_dir, "refs/")?;
    all_refs.sort_by(|a, b| a.0.cmp(&b.0));

    for (refname, oid) in all_refs {
        // Apply `--decorate-refs` / `--decorate-refs-exclude` / `log.excludeDecoration`.
        if !filter.is_empty() && !refname.starts_with(&rep_base) && !filter.matches(&refname) {
            continue;
        }
        if refname.starts_with(&rep_base) {
            let Some(rest) = refname.strip_prefix(&rep_base) else {
                continue;
            };
            let rest = rest.trim();
            if rest.len() != 40 || rest.parse::<ObjectId>().is_err() {
                continue;
            }
            prepend_if_target(
                &mut map,
                rest.to_owned(),
                commit_targets,
                DecorationItem {
                    refname: None,
                    display: "replaced".to_owned(),
                    kind: DecorationKind::Grafted,
                },
            );
            continue;
        }

        if refname == "refs/stash" || refname.starts_with("refs/stash/") {
            let hex = peel_to_commit_hex(odb, &oid.to_hex()).unwrap_or_else(|| oid.to_hex());
            prepend_if_target(
                &mut map,
                hex,
                commit_targets,
                DecorationItem {
                    refname: Some("refs/stash".to_string()),
                    display: "refs/stash".to_owned(),
                    kind: DecorationKind::Stash,
                },
            );
            continue;
        }

        if let Some(rest) = refname.strip_prefix("refs/heads/") {
            let hex = oid.to_hex();
            if commit_targets.is_some_and(|targets| !targets.contains(&hex)) {
                continue;
            }
            let display = if full {
                refname.clone()
            } else {
                rest.to_owned()
            };
            prepend_if_target(
                &mut map,
                hex,
                commit_targets,
                DecorationItem {
                    refname: Some(refname.clone()),
                    display,
                    kind: DecorationKind::Branch,
                },
            );
            continue;
        }

        if let Some(rest) = refname.strip_prefix("refs/tags/") {
            let display = if full {
                refname.clone()
            } else {
                rest.to_owned()
            };
            let direct = oid.to_hex();
            let peeled = if commit_targets.is_some_and(|targets| targets.contains(&direct)) {
                direct
            } else {
                commit_hex_for_ref_object(odb, &oid)
            };
            if commit_targets.is_some_and(|targets| !targets.contains(&peeled)) {
                continue;
            }
            prepend_if_target(
                &mut map,
                peeled,
                commit_targets,
                DecorationItem {
                    refname: Some(refname.clone()),
                    display,
                    kind: DecorationKind::Tag,
                },
            );
            continue;
        }

        if let Some(rest) = refname.strip_prefix("refs/remotes/") {
            let hex = oid.to_hex();
            if commit_targets.is_some_and(|targets| !targets.contains(&hex)) {
                continue;
            }
            let display = if full {
                refname.clone()
            } else {
                rest.to_owned()
            };
            prepend_if_target(
                &mut map,
                hex,
                commit_targets,
                DecorationItem {
                    refname: Some(refname.clone()),
                    display,
                    kind: DecorationKind::RemoteBranch,
                },
            );
            continue;
        }

        // Refs outside the known namespaces (e.g. `refs/hidden/*`, `refs/prefetch/*`,
        // `refs/notes/commits`). Git's `set_default_decoration_filter` (builtin/log.c)
        // only restricts decorations to the known namespaces when NO `--decorate-refs`,
        // `--decorate-refs-exclude`, or `log.excludeDecoration` is given. As soon as any
        // such filter is active (or `--clear-decorations` is set), the default include
        // list is dropped and every ref that passes the filter is decorated with its full
        // name and `DECORATION_NONE` (reset) color (log-tree.c `add_ref_decoration`).
        if clear_decorations || !filter.is_empty() {
            let peeled = peel_to_commit_hex(odb, &oid.to_hex()).unwrap_or_else(|| oid.to_hex());
            prepend_if_target(
                &mut map,
                peeled,
                commit_targets,
                DecorationItem {
                    refname: Some(refname.clone()),
                    display: refname.clone(),
                    kind: DecorationKind::Other,
                },
            );
        }
    }

    if let Some(oid) = head.oid() {
        if filter.is_empty() || filter.matches("HEAD") {
            let hex = oid.to_hex();
            prepend_if_target(
                &mut map,
                hex,
                commit_targets,
                DecorationItem {
                    refname: Some("HEAD".to_string()),
                    display: "HEAD".to_owned(),
                    kind: DecorationKind::Head,
                },
            );
        }
    }

    for items in map.values_mut() {
        // Dedup truly identical decorations (same refname). A tag and a branch
        // that happen to share a short name (e.g. `tag: reach` and `reach`) are
        // distinct and both shown, matching Git.
        let mut seen = HashSet::new();
        items.retain(|it| seen.insert((it.kind, it.display.clone(), it.refname.clone())));
        if hide_remote_update_noise {
            let branch_names: HashSet<String> = items
                .iter()
                .filter(|it| it.kind == DecorationKind::Branch)
                .map(|it| it.display.clone())
                .collect();
            if !branch_names.is_empty() {
                let hide_detached_head = !matches!(head, HeadState::Branch { .. });
                items.retain(|it| {
                    if hide_detached_head && it.kind == DecorationKind::Head {
                        return false;
                    }
                    if it.kind == DecorationKind::RemoteBranch {
                        let short_remote = it
                            .display
                            .split_once('/')
                            .map(|(_, branch)| branch)
                            .unwrap_or(it.display.as_str());
                        return !branch_names.contains(short_remote) && short_remote != "HEAD";
                    }
                    true
                });
            }
        }
    }

    Ok(map)
}

/// Map a ref's peeled object id to the commit hex used for decoration lookup.
fn commit_hex_for_ref_object(odb: &Odb, oid: &ObjectId) -> String {
    peel_to_commit_hex(odb, &oid.to_hex()).unwrap_or_else(|| oid.to_hex())
}

/// Peel an object (possibly a tag) down to a commit and return its hex.
pub fn peel_to_commit_hex(odb: &Odb, hex: &str) -> Option<String> {
    let oid: ObjectId = hex.parse().ok()?;
    let obj = odb.read(&oid).ok()?;
    match obj.kind {
        ObjectKind::Commit => Some(hex.to_owned()),
        ObjectKind::Tag => {
            let text = std::str::from_utf8(&obj.data).ok()?;
            for line in text.lines() {
                if let Some(target) = line.strip_prefix("object ") {
                    let target_hex = target.trim();
                    return peel_to_commit_hex(odb, target_hex);
                }
            }
            None
        }
        _ => None,
    }
}

/// Index of the branch decoration that should fold into `HEAD -> branch`, if any.
///
/// Returns the position of the current-branch [`DecorationItem`] in `items` when a
/// `HEAD` decoration is actually present (so the CLI can render `HEAD -> branch`),
/// or `None` when HEAD is detached or was filtered out.
pub fn current_branch_decoration_index(
    items: &[DecorationItem],
    head: &HeadState,
) -> Option<usize> {
    let refname = match head {
        HeadState::Branch { refname, .. } => refname.as_str(),
        _ => return None,
    };
    // Only fold the branch into `HEAD -> branch` when the HEAD decoration is
    // actually present. If HEAD was filtered out (e.g. by `--decorate-refs`),
    // the branch must render on its own.
    if !items.iter().any(|it| it.kind == DecorationKind::Head) {
        return None;
    }
    items
        .iter()
        .position(|it| it.kind == DecorationKind::Branch && it.refname.as_deref() == Some(refname))
}
