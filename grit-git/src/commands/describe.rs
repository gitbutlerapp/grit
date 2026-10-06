//! `grit describe` — give a human-readable name to a commit based on the nearest tag.
//!
//! Walks backwards from a commit via BFS to find the most recent reachable tag,
//! then outputs `<tag>-<n>-g<abbrev>` where n is the number of commits since
//! that tag and abbrev is the abbreviated commit SHA.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions,
};
use grit_lib::error::Error;
use grit_lib::index::Index;
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::refs::list_refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::state::resolve_head;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::Path;

/// Arguments for `grit describe`.
#[derive(Debug, ClapArgs)]
#[command(about = "Give an object a human readable name based on an available ref")]
pub struct Args {
    /// Commit-ish to describe (defaults to HEAD).
    #[arg()]
    pub commit: Option<String>,

    /// Instead of using only annotated tags, use any tag found in `refs/tags/`.
    #[arg(long)]
    pub tags: bool,

    /// If no tag is found, show the abbreviated commit object as fallback.
    #[arg(long)]
    pub always: bool,

    /// Always output the long format (the tag, the number of commits, and the
    /// abbreviated commit name) even when it matches a tag.
    #[arg(long)]
    pub long: bool,

    /// Do not force long output.
    #[arg(long = "no-long", action = clap::ArgAction::SetTrue)]
    pub no_long: bool,

    /// Use <n> digits (or as many as needed) to form the abbreviated object
    /// name. A value of 0 suppresses the long format.
    #[arg(long, default_value = "7")]
    pub abbrev: usize,

    /// Show unabbreviated object names.
    #[arg(long = "no-abbrev", action = clap::ArgAction::SetTrue)]
    pub no_abbrev: bool,

    /// Instead of considering only the 10 most recent tags as candidates,
    /// consider this many. Increasing above 10 takes proportionally longer
    /// but may give a more accurate result.
    #[arg(long, default_value = "10")]
    pub candidates: usize,

    /// Only consider tags matching the given glob(7) pattern.
    #[arg(long = "match")]
    pub match_pattern: Vec<String>,

    /// Clear accumulated --match patterns.
    #[arg(long = "no-match", action = clap::ArgAction::SetTrue)]
    pub no_match: bool,

    /// Do not consider tags matching the given glob(7) pattern.
    #[arg(long = "exclude")]
    pub exclude_pattern: Vec<String>,

    /// Clear any previous --exclude patterns.
    #[arg(long = "no-exclude", action = clap::ArgAction::SetTrue)]
    pub no_exclude: bool,

    /// Only output exact matches (a tag directly references the commit).
    #[arg(long)]
    pub exact_match: bool,

    /// Do not limit the search to exact matches.
    #[arg(long = "no-exact-match", action = clap::ArgAction::SetTrue)]
    pub no_exact_match: bool,

    /// Display the first-parent chain only.
    #[arg(long)]
    pub first_parent: bool,

    /// Instead of using only annotated tags, use any ref found in
    /// `refs/heads/` and `refs/remotes/` in addition to `refs/tags/`.
    #[arg(long)]
    pub all: bool,

    /// Instead of finding the tag that is an ancestor, find the tag
    /// that contains the commit (i.e., is a descendant).
    #[arg(long)]
    pub contains: bool,

    /// Describe the working tree.  After the version string, append
    /// the given mark (default: "-dirty") if the working tree has
    /// local modifications.
    #[arg(long, default_missing_value = "-dirty", num_args = 0..=1, require_equals = true)]
    pub dirty: Option<String>,

    /// Describe the working tree.  After the version string, append
    /// the given mark (default: "-broken") if the working tree cannot
    /// be described (e.g. HEAD points to a broken commit).
    #[arg(long, default_missing_value = "-broken", num_args = 0..=1, require_equals = true)]
    pub broken: Option<String>,
}

/// A candidate tag found during the BFS walk.
#[derive(Debug, Clone)]
struct Candidate {
    /// The short tag name (e.g. `v1.0`).
    tag_name: String,
    /// Object ID of the selected tag's peeled commit.
    tag_oid: ObjectId,
    /// Ref name when the annotated tag object's embedded name differs.
    misnamed_ref: Option<String>,
    /// Number of commits between the tagged commit and the target.
    depth: usize,
    /// Number of commits reachable from the target but not from the tag.
    different_commits: usize,
}

#[derive(Debug, Clone)]
struct RefCandidate {
    name: String,
    annotated: bool,
    tagger_time: i64,
    misnamed_ref: Option<String>,
}

/// Options controlling how a commit-ish is described.
#[derive(Debug, Clone)]
pub(crate) struct DescribeOptions {
    /// Include lightweight tags in addition to annotated tags.
    pub(crate) tags: bool,
    /// Fall back to an abbreviated object name when no tag describes the object.
    pub(crate) always: bool,
    /// Always print the long `<name>-<count>-g<oid>` form.
    pub(crate) long: bool,
    /// Minimum object-name abbreviation length to print.
    pub(crate) abbrev: usize,
    /// Maximum number of candidate tags to consider during the walk.
    pub(crate) candidates: usize,
    /// Tag glob patterns to include.
    pub(crate) match_pattern: Vec<String>,
    /// Tag glob patterns to exclude.
    pub(crate) exclude_pattern: Vec<String>,
    /// Require the tag to point directly at the target commit.
    pub(crate) exact_match: bool,
    /// Walk only the first-parent chain.
    pub(crate) first_parent: bool,
    /// Include heads and remote-tracking refs in addition to tags.
    pub(crate) all: bool,
    /// Describe by finding a ref that contains the target commit.
    pub(crate) contains: bool,
}

impl DescribeOptions {
    /// Default options used for the `%(describe...)` pretty placeholder
    /// (`git log --format`), matching Git's `format_describe` defaults.
    #[must_use]
    pub(crate) fn default_for_format() -> Self {
        Self {
            tags: false,
            always: false,
            long: false,
            abbrev: 7,
            candidates: 10,
            match_pattern: Vec::new(),
            exclude_pattern: Vec::new(),
            exact_match: false,
            first_parent: false,
            all: false,
            contains: false,
        }
    }

    /// Build describe options from parsed CLI arguments.
    #[must_use]
    pub(crate) fn from_args(args: &Args) -> Self {
        Self {
            tags: args.tags,
            always: args.always,
            long: args.long && !args.no_long,
            abbrev: if args.no_abbrev { 40 } else { args.abbrev },
            candidates: args.candidates,
            match_pattern: if args.no_match {
                Vec::new()
            } else {
                args.match_pattern.clone()
            },
            exclude_pattern: if args.no_exclude {
                Vec::new()
            } else {
                args.exclude_pattern.clone()
            },
            exact_match: args.exact_match && !args.no_exact_match,
            first_parent: args.first_parent,
            all: args.all,
            contains: args.contains,
        }
    }
}

/// Run the `describe` command.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;

    if args.commit.is_some() && (args.dirty.is_some() || args.broken.is_some()) {
        bail!("--dirty/--broken cannot be used with commit-ish arguments");
    }

    // --broken: if HEAD cannot be resolved, output the broken suffix and return
    if args.broken.is_some() {
        let rev = args.commit.as_deref().unwrap_or("HEAD");
        let broken_suffix = args.broken.as_deref().unwrap_or("-broken");
        match resolve_revision(&repo, rev) {
            Ok(oid) => {
                if peel_to_commit(&repo, &oid).is_none() {
                    // HEAD is not a valid commit
                    let abbrev = abbreviate(&oid, if args.no_abbrev { 40 } else { args.abbrev });
                    println!("{abbrev}{broken_suffix}");
                    return Ok(());
                }
            }
            Err(_) => {
                // Can't even resolve HEAD
                println!("HEAD{broken_suffix}");
                return Ok(());
            }
        }
    }

    // Resolve the target commit
    let rev = args.commit.as_deref().unwrap_or("HEAD");
    let resolved_oid =
        resolve_revision(&repo, rev).with_context(|| format!("Not a valid object name {rev}"))?;

    let mut options = DescribeOptions::from_args(&args);
    apply_ordered_pattern_options_from_argv(&mut options);

    // Peel to commit (in case user passed a tag name which resolves to a tag object), or describe
    // a blob by finding the first reachable commit/path that contains it.
    let target_oid = if let Some(commit_oid) = peel_to_commit(&repo, &resolved_oid) {
        commit_oid
    } else {
        let obj = repo.odb.read(&resolved_oid)?;
        if obj.kind == ObjectKind::Blob {
            let description = describe_blob(&repo, &resolved_oid, &options)?;
            println!("{description}");
            return Ok(());
        }
        bail!("fatal: {rev} is neither a commit nor blob");
    };

    // Determine the dirty suffix before formatting the final description.
    let dirty_suffix = if args.dirty.is_some() || args.broken.is_some() {
        match is_worktree_dirty(&repo) {
            Ok(true) if args.dirty.is_some() => {
                args.dirty.as_deref().unwrap_or("-dirty").to_string()
            }
            Ok(_) => String::new(),
            Err(_) if args.broken.is_some() => {
                args.broken.as_deref().unwrap_or("-broken").to_string()
            }
            Err(err) => return Err(err.into()),
        }
    } else {
        String::new()
    };

    let description = describe_commit(&repo, target_oid, &options, &dirty_suffix)?;
    println!("{description}");

    Ok(())
}

fn apply_ordered_pattern_options_from_argv(options: &mut DescribeOptions) {
    let argv: Vec<String> = env::args().collect();
    let Some(describe_idx) = argv.iter().position(|arg| arg == "describe") else {
        return;
    };

    let mut match_pattern = Vec::new();
    let mut exclude_pattern = Vec::new();
    let mut i = describe_idx + 1;
    while i < argv.len() {
        let arg = &argv[i];
        if let Some(value) = arg.strip_prefix("--match=") {
            match_pattern.push(value.to_owned());
        } else if arg == "--match" {
            if let Some(value) = argv.get(i + 1) {
                match_pattern.push(value.to_owned());
                i += 1;
            }
        } else if arg == "--no-match" {
            match_pattern.clear();
        } else if let Some(value) = arg.strip_prefix("--exclude=") {
            exclude_pattern.push(value.to_owned());
        } else if arg == "--exclude" {
            if let Some(value) = argv.get(i + 1) {
                exclude_pattern.push(value.to_owned());
                i += 1;
            }
        } else if arg == "--no-exclude" {
            exclude_pattern.clear();
        }
        i += 1;
    }

    options.match_pattern = match_pattern;
    options.exclude_pattern = exclude_pattern;
}

/// Describe a resolved object using Git-compatible `describe` semantics.
///
/// The object may be a commit or a tag object that peels to a commit. The returned
/// string does not include a trailing newline.
///
/// # Errors
///
/// Returns an error when the object cannot be peeled to a commit, no suitable ref
/// exists and `always` is false, or repository objects cannot be read.
pub(crate) fn describe_object(
    repo: &Repository,
    oid: ObjectId,
    options: &DescribeOptions,
) -> Result<String> {
    let target_oid = peel_to_commit(repo, &oid)
        .ok_or_else(|| anyhow::anyhow!("Not a valid commit: {}", oid.to_hex()))?;
    describe_commit(repo, target_oid, options, "")
}

fn describe_commit(
    repo: &Repository,
    target_oid: ObjectId,
    options: &DescribeOptions,
    dirty_suffix: &str,
) -> Result<String> {
    // Build a map from commit OID -> ref name for all qualifying refs.
    // `git describe --contains` considers lightweight tags too (see submodule--helper
    // `compute_rev_name`, which runs `describe --contains` as the third attempt).
    let use_all_tags = options.tags || options.contains;
    let ref_map = build_ref_map(
        repo,
        use_all_tags,
        options.all,
        &options.match_pattern,
        &options.exclude_pattern,
    )?;

    if options.contains {
        return describe_contains(repo, &target_oid, &ref_map);
    }

    // Check if the target commit itself is tagged (exact match).
    if let Some(ref_candidate) = ref_map.get(&target_oid) {
        if options.long || ref_candidate.misnamed_ref.is_some() {
            let abbrev = abbreviate(&target_oid, options.abbrev);
            return Ok(format!("{}-0-g{abbrev}{dirty_suffix}", ref_candidate.name));
        } else {
            return Ok(format!("{}{dirty_suffix}", ref_candidate.name));
        }
    }

    // If --exact-match, we must have found it above.
    if options.exact_match {
        bail!("no tag exactly matches '{}'", target_oid.to_hex());
    }

    // BFS walk backwards from target to find the nearest tagged ancestor.
    let candidate = bfs_find_tag(
        repo,
        &target_oid,
        &ref_map,
        options.candidates,
        options.first_parent,
    )?;

    match candidate {
        Some(c) => {
            if let Some(ref_name) = c.misnamed_ref {
                eprintln!(
                    "warning: tag '{ref_name}' is externally known as '{}'",
                    c.tag_name
                );
            }
            let abbrev = abbreviate(&target_oid, options.abbrev);
            Ok(format!(
                "{}-{}-g{abbrev}{dirty_suffix}",
                c.tag_name, c.different_commits
            ))
        }
        None => {
            if options.always {
                let abbrev = abbreviate(&target_oid, options.abbrev);
                Ok(format!("{abbrev}{dirty_suffix}"))
            } else {
                bail!(
                    "No names found, cannot describe anything.\n\
                     \n\
                     How would you describe a commit without any tags?\n\
                     Use --always to fall back to abbreviated commit."
                );
            }
        }
    }
}

fn describe_blob(
    repo: &Repository,
    blob_oid: &ObjectId,
    options: &DescribeOptions,
) -> Result<String> {
    let head = resolve_head(&repo.git_dir).map_err(|_| {
        anyhow::anyhow!(
            "fatal: cannot search for blob '{}' on an unborn branch",
            blob_oid.to_hex()
        )
    })?;
    let head_oid = head.oid().ok_or_else(|| {
        anyhow::anyhow!(
            "fatal: cannot search for blob '{}' on an unborn branch",
            blob_oid.to_hex()
        )
    })?;

    let mut commits = Vec::new();
    let mut queue = VecDeque::from([*head_oid]);
    let mut seen = HashSet::from([*head_oid]);
    while let Some(oid) = queue.pop_front() {
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        commits.push((oid, commit.tree));
        for parent in commit.parents {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }

    for (commit_oid, tree_oid) in commits.into_iter().rev() {
        if let Some(path) = find_blob_path_in_tree(repo, &tree_oid, blob_oid, "")? {
            let commit_desc = describe_commit(repo, commit_oid, options, "")?;
            return Ok(format!("{commit_desc}:{path}"));
        }
    }

    bail!(
        "fatal: blob '{}' not reachable from HEAD",
        blob_oid.to_hex()
    );
}

fn find_blob_path_in_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    blob_oid: &ObjectId,
    prefix: &str,
) -> Result<Option<String>> {
    let obj = repo.odb.read(tree_oid)?;
    let entries = parse_tree(&obj.data)?;
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.oid == *blob_oid && entry.mode != grit_lib::index::MODE_TREE {
            return Ok(Some(path));
        }
        if entry.mode == grit_lib::index::MODE_TREE {
            if let Some(found) = find_blob_path_in_tree(repo, &entry.oid, blob_oid, &path)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// Check if the working tree has uncommitted changes.
/// --contains: find the nearest tag that is a descendant of (contains) the target commit.
/// Walk forward from each tag's commit to check if the target is an ancestor.
fn describe_contains(
    repo: &Repository,
    target_oid: &ObjectId,
    ref_map: &HashMap<ObjectId, RefCandidate>,
) -> Result<String> {
    // For each tag, check if target is reachable from the tag commit.
    // Track the best (shortest path) tag.
    let mut best: Option<(RefCandidate, usize)> = None;

    for (tag_oid, candidate) in ref_map {
        if let Some(depth) = ancestor_depth(repo, tag_oid, target_oid) {
            if best.as_ref().is_none_or(|(_, d)| depth < *d) {
                best = Some((candidate.clone(), depth));
            }
        }
    }

    match best {
        Some((candidate, depth)) => {
            if depth == 0 {
                if candidate.annotated {
                    Ok(format!("{}^0", candidate.name))
                } else {
                    Ok(candidate.name)
                }
            } else {
                Ok(format!("{}~{depth}", candidate.name))
            }
        }
        None => {
            bail!("fatal: cannot describe '{}'", target_oid.to_hex());
        }
    }
}

/// Check if `ancestor` is reachable from `descendant` by walking parents.
/// Returns Some(depth) if reachable, None otherwise.
fn ancestor_depth(repo: &Repository, descendant: &ObjectId, ancestor: &ObjectId) -> Option<usize> {
    if descendant == ancestor {
        return Some(0);
    }
    let mut queue: VecDeque<(ObjectId, usize)> = VecDeque::new();
    let mut visited = HashSet::new();
    queue.push_back((*descendant, 0));
    visited.insert(*descendant);

    while let Some((oid, depth)) = queue.pop_front() {
        let obj = repo.odb.read(&oid).ok()?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data).ok()?;
        for parent in &commit.parents {
            if parent == ancestor {
                return Some(depth + 1);
            }
            if visited.insert(*parent) {
                queue.push_back((*parent, depth + 1));
            }
        }
    }
    None
}

fn is_worktree_dirty(repo: &Repository) -> grit_lib::error::Result<bool> {
    let workdir = match &repo.work_tree {
        Some(d) => d,
        None => return Ok(false),
    };
    let head = match resolve_head(&repo.git_dir) {
        Ok(h) => h,
        Err(_) => return Ok(true),
    };
    let index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };
    let head_tree = match head.oid() {
        Some(oid) => match repo.odb.read(oid) {
            Ok(obj) => match parse_commit(&obj.data) {
                Ok(c) => Some(c.tree),
                Err(_) => return Ok(true),
            },
            Err(_) => return Ok(true),
        },
        None => None,
    };
    let staged = diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), false)?;
    if !staged.is_empty() {
        return Ok(true);
    }
    diff_index_to_worktree_with_options(
        &repo.odb,
        &index,
        workdir,
        DiffIndexToWorktreeOptions {
            error_on_broken_gitlinks: true,
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .map(|u| !u.is_empty())
}

/// Build a map from commit OID to ref metadata for all qualifying refs.
///
/// - `use_all_tags`: include lightweight tags (not just annotated).
/// - `use_all_refs`: include refs/heads/ and refs/remotes/ too (--all).
/// - If `patterns` is non-empty, only refs whose short name matches one of the
///   glob patterns are included.
/// - If `exclude_patterns` is non-empty, refs whose short name matches one of
///   those glob patterns are omitted.
fn build_ref_map(
    repo: &Repository,
    use_all_tags: bool,
    use_all_refs: bool,
    patterns: &[String],
    exclude_patterns: &[String],
) -> Result<HashMap<ObjectId, RefCandidate>> {
    let mut map: HashMap<ObjectId, RefCandidate> = HashMap::new();

    // Collect all refs under refs/tags/ (loose)
    let loose_tags = list_refs(&repo.git_dir, "refs/tags/").unwrap_or_default();

    // Also collect tags from packed-refs
    let packed_tags = read_packed_tags(&repo.git_dir)?;

    // Merge: loose refs take priority
    let mut all_tags: BTreeMap<String, ObjectId> = BTreeMap::new();
    for (refname, oid) in packed_tags {
        all_tags.insert(refname, oid);
    }
    for (refname, oid) in loose_tags {
        all_tags.insert(refname, oid);
    }

    for (refname, oid) in &all_tags {
        // When --all is active, preserve the `tags/` prefix (strip only `refs/`)
        // to match git's behavior. Otherwise, strip `refs/tags/` entirely.
        let short_name = if use_all_refs {
            refname.strip_prefix("refs/").unwrap_or(refname).to_string()
        } else {
            refname
                .strip_prefix("refs/tags/")
                .unwrap_or(refname)
                .to_string()
        };

        // Filter by glob patterns
        if !patterns.is_empty()
            && !patterns
                .iter()
                .any(|p| crate::commands::tag::glob_matches(p, &short_name))
        {
            continue;
        }
        if exclude_patterns
            .iter()
            .any(|p| crate::commands::tag::glob_matches(p, &short_name))
        {
            continue;
        }

        // Read the object to check if it's an annotated tag or a direct commit ref
        let obj = match repo.odb.read(oid) {
            Ok(o) => o,
            Err(_) => continue,
        };

        match obj.kind {
            ObjectKind::Tag => {
                // Annotated tag — peel to commit
                if let Ok(tag_data) = parse_tag(&obj.data) {
                    if let Some(commit_oid) = peel_to_commit(repo, &tag_data.object) {
                        let (display_name, misnamed_ref) = if use_all_refs {
                            (short_name.clone(), None)
                        } else {
                            (
                                tag_data.tag.clone(),
                                (tag_data.tag != short_name).then(|| short_name.clone()),
                            )
                        };
                        insert_ref_candidate(
                            &mut map,
                            commit_oid,
                            RefCandidate {
                                name: display_name,
                                annotated: true,
                                tagger_time: tag_data
                                    .tagger
                                    .as_deref()
                                    .and_then(tagger_timestamp)
                                    .unwrap_or(0),
                                misnamed_ref,
                            },
                        );
                    }
                }
            }
            ObjectKind::Commit if use_all_tags || use_all_refs => {
                // Lightweight tag pointing directly at a commit
                insert_ref_candidate(
                    &mut map,
                    *oid,
                    RefCandidate {
                        name: short_name.clone(),
                        annotated: false,
                        tagger_time: 0,
                        misnamed_ref: None,
                    },
                );
            }
            _ => {}
        }
    }

    // --all: also include branches and remote tracking refs
    if use_all_refs {
        for prefix in &["refs/heads/", "refs/remotes/"] {
            let refs = list_refs(&repo.git_dir, prefix).unwrap_or_default();
            for (refname, oid) in &refs {
                // Display name for --all is the refname with "refs/" stripped
                let display = refname.strip_prefix("refs/").unwrap_or(refname).to_string();
                let match_name = refname.strip_prefix(prefix).unwrap_or(refname);

                if !patterns.is_empty()
                    && !patterns
                        .iter()
                        .any(|p| crate::commands::tag::glob_matches(p, match_name))
                {
                    continue;
                }
                if exclude_patterns
                    .iter()
                    .any(|p| crate::commands::tag::glob_matches(p, match_name))
                {
                    continue;
                }

                // Peel to commit
                if let Some(commit_oid) = peel_to_commit(repo, oid) {
                    insert_ref_candidate(
                        &mut map,
                        commit_oid,
                        RefCandidate {
                            name: display.clone(),
                            annotated: false,
                            tagger_time: 0,
                            misnamed_ref: None,
                        },
                    );
                }
            }
        }

        if patterns.is_empty() && exclude_patterns.is_empty() {
            let refs = list_refs(&repo.git_dir, "refs/original/").unwrap_or_default();
            for (refname, oid) in &refs {
                let display = refname.strip_prefix("refs/").unwrap_or(refname).to_string();

                if let Some(commit_oid) = peel_to_commit(repo, oid) {
                    insert_ref_candidate(
                        &mut map,
                        commit_oid,
                        RefCandidate {
                            name: display.clone(),
                            annotated: false,
                            tagger_time: 0,
                            misnamed_ref: None,
                        },
                    );
                }
            }
        }
    }

    Ok(map)
}

fn insert_ref_candidate(
    map: &mut HashMap<ObjectId, RefCandidate>,
    oid: ObjectId,
    candidate: RefCandidate,
) {
    match map.get_mut(&oid) {
        None => {
            map.insert(oid, candidate);
        }
        Some(existing) => {
            let replace = if candidate.annotated != existing.annotated {
                candidate.annotated
            } else if candidate.tagger_time != existing.tagger_time {
                candidate.tagger_time > existing.tagger_time
            } else {
                candidate.name > existing.name
            };
            if replace {
                *existing = candidate;
            }
        }
    }
}

fn tagger_timestamp(raw: &str) -> Option<i64> {
    let after_email = raw.split_once('>')?.1.trim();
    after_email.split_whitespace().next()?.parse().ok()
}

/// Read tag refs from packed-refs file.
fn read_packed_tags(git_dir: &Path) -> Result<Vec<(String, ObjectId)>> {
    let packed_path = git_dir.join("packed-refs");
    let content = match fs::read_to_string(&packed_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut results = Vec::new();
    for line in content.lines() {
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let hash = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("").trim();
        if name.starts_with("refs/tags/") && hash.len() == 40 {
            if let Ok(oid) = hash.parse::<ObjectId>() {
                results.push((name.to_string(), oid));
            }
        }
    }
    Ok(results)
}

/// Peel an object to a commit OID (following tag objects).
fn peel_to_commit(repo: &Repository, oid: &ObjectId) -> Option<ObjectId> {
    let mut current = *oid;
    for _ in 0..20 {
        let obj = repo.odb.read(&current).ok()?;
        match obj.kind {
            ObjectKind::Commit => return Some(current),
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data).ok()?;
                current = tag.object;
            }
            _ => return None,
        }
    }
    None
}

/// BFS walk backwards from `start` to find the nearest tagged commit.
///
/// Returns `None` if no tagged ancestor is found.
fn bfs_find_tag(
    repo: &Repository,
    start: &ObjectId,
    tag_map: &HashMap<ObjectId, RefCandidate>,
    max_candidates: usize,
    first_parent: bool,
) -> Result<Option<Candidate>> {
    // BFS with distance tracking
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<(ObjectId, usize)> = VecDeque::new();
    let mut candidates: Vec<Candidate> = Vec::new();

    queue.push_back((*start, 0));
    visited.insert(*start);

    while let Some((oid, depth)) = queue.pop_front() {
        // If we already have enough candidates and this depth exceeds the worst,
        // we can stop.
        if candidates.len() >= max_candidates {
            // All candidates at this point are at depth <= this depth,
            // since BFS explores in order. We can stop.
            break;
        }

        // Read the commit
        let obj = match repo.odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };

        if obj.kind != ObjectKind::Commit {
            continue;
        }

        let commit = match parse_commit(&obj.data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Check parents for tags
        let parents = if first_parent {
            commit.parents.into_iter().take(1).collect::<Vec<_>>()
        } else {
            commit.parents
        };

        for parent_oid in parents {
            if !visited.insert(parent_oid) {
                continue;
            }

            let parent_depth = depth + 1;

            if let Some(tag_candidate) = tag_map.get(&parent_oid) {
                candidates.push(Candidate {
                    tag_name: tag_candidate.name.clone(),
                    tag_oid: parent_oid,
                    misnamed_ref: tag_candidate.misnamed_ref.clone(),
                    depth: parent_depth,
                    different_commits: count_reachable_difference(
                        repo,
                        start,
                        &parent_oid,
                        first_parent,
                    )?,
                });
                if candidates.len() >= max_candidates {
                    break;
                }
                // Don't enqueue this commit's parents — we found a tag here
                // but we continue BFS to find potentially closer tags on other branches
                continue;
            }

            queue.push_back((parent_oid, parent_depth));
        }
    }

    candidates.sort_by(|left, right| {
        left.different_commits
            .cmp(&right.different_commits)
            .then(left.depth.cmp(&right.depth))
            .then(left.tag_name.cmp(&right.tag_name))
            .then(left.tag_oid.cmp(&right.tag_oid))
    });
    Ok(candidates.into_iter().next())
}

fn count_reachable_difference(
    repo: &Repository,
    target: &ObjectId,
    base: &ObjectId,
    first_parent: bool,
) -> Result<usize> {
    let base_commits = collect_reachable_commits(repo, base, first_parent)?;
    let target_commits = collect_reachable_commits(repo, target, first_parent)?;
    Ok(target_commits.difference(&base_commits).count())
}

fn collect_reachable_commits(
    repo: &Repository,
    start: &ObjectId,
    first_parent: bool,
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([*start]);

    while let Some(oid) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        let parents: Box<dyn Iterator<Item = ObjectId>> = if first_parent {
            Box::new(commit.parents.into_iter().take(1))
        } else {
            Box::new(commit.parents.into_iter())
        };
        queue.extend(parents);
    }

    Ok(seen)
}

/// Abbreviate an OID to `n` hex characters.
fn abbreviate(oid: &ObjectId, n: usize) -> String {
    let hex = oid.to_hex();
    if n == 0 {
        return String::new();
    }
    hex[..n.min(40)].to_string()
}
