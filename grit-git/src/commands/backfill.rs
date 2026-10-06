//! `grit backfill` — download missing blobs for partial clone.
//!
//! Walks reachable trees from `HEAD`, batches missing blob OIDs recorded in
//! `grit-promisor-missing`, copies their contents from the promisor remote's
//! object store, and emits trace2 `data` lines compatible with upstream tests.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::promisor::read_promisor_missing_oids;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use std::collections::{HashSet, VecDeque};
use std::fs;

use super::promisor_hydrate::{find_promisor_source, flush_promisor_blob_batch, PromisorSource};

/// Arguments for `grit backfill`.
#[derive(Debug, ClapArgs)]
#[command(about = "Download missing blobs for partial clone")]
pub struct Args {
    /// Limit backfill to a pathspec.
    #[arg(value_name = "PATH", num_args = 0..)]
    pub paths: Vec<String>,

    /// Minimum batch size for fetching.
    #[arg(long = "min-batch-size")]
    pub min_batch_size: Option<usize>,

    /// Restrict missing objects to sparse-checkout paths (like `git backfill --sparse`).
    #[arg(long = "sparse")]
    pub sparse: bool,

    /// Fetch all reachable blobs, ignoring sparse-checkout.
    #[arg(long = "no-sparse", conflicts_with = "sparse")]
    pub no_sparse: bool,
}

/// Run `grit backfill`.
pub fn run(args: Args) -> Result<()> {
    if !args.paths.is_empty() {
        bail!("grit backfill: path arguments are not supported yet");
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let sparse_enabled_cfg = config
        .get("core.sparseCheckout")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let sparse_mode = if args.no_sparse {
        false
    } else if args.sparse {
        true
    } else {
        sparse_enabled_cfg
    };

    let min_batch = args.min_batch_size.unwrap_or(50_000).max(1);

    let mut marker_oids: HashSet<ObjectId> = read_promisor_missing_oids(&repo.git_dir)
        .into_iter()
        .collect();
    let use_marker_filter = !marker_oids.is_empty();

    let (patterns, cone_mode) = if sparse_mode {
        let sc_path = repo.git_dir.join("info").join("sparse-checkout");
        let content = fs::read_to_string(&sc_path)
            .map_err(|_| anyhow::anyhow!("problem loading sparse-checkout"))?;
        let patterns: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect();
        let cone = config
            .get("core.sparseCheckoutCone")
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true);
        (Some(patterns), cone)
    } else {
        (None, true)
    };

    let promisor = match find_promisor_source(&config, &repo.git_dir)? {
        Some(p) => p,
        None => {
            if marker_oids.is_empty() {
                return Ok(());
            }
            bail!("no promisor remote configured");
        }
    };

    let head_state = resolve_head(&repo.git_dir).context("reading HEAD")?;
    let head_oid = match head_state {
        grit_lib::state::HeadState::Detached { oid } => oid,
        grit_lib::state::HeadState::Branch { ref refname, .. } => {
            refs::resolve_ref(&repo.git_dir, refname)
                .with_context(|| format!("resolving {refname}"))?
        }
        grit_lib::state::HeadState::Invalid => {
            bail!("HEAD is in an invalid state");
        }
    };

    let head_obj = repo.odb.read(&head_oid).context("reading HEAD object")?;
    if head_obj.kind != ObjectKind::Commit {
        bail!("HEAD does not point to a commit");
    }

    let mut batch: Vec<ObjectId> = Vec::new();
    let mut paths_visited: usize = 0;
    let mut path_trace_opt: Option<HashSet<String>> = if sparse_mode {
        Some(HashSet::new())
    } else {
        None
    };
    let mut seen_commits = HashSet::new();
    let mut seen_trees = HashSet::new();
    let mut queued_blobs = HashSet::new();
    let mut commit_queue = VecDeque::new();
    commit_queue.push_back(head_oid);

    while let Some(cid) = commit_queue.pop_front() {
        if !seen_commits.insert(cid) {
            continue;
        }
        let obj = match repo.odb.read(&cid) {
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
        for p in &commit.parents {
            commit_queue.push_back(*p);
        }
        walk_tree_for_backfill(
            &repo,
            &promisor,
            commit.tree,
            "",
            sparse_mode,
            patterns.as_deref(),
            cone_mode,
            &mut marker_oids,
            use_marker_filter,
            min_batch,
            &mut batch,
            &mut paths_visited,
            &mut path_trace_opt,
            &mut seen_trees,
            &mut queued_blobs,
        )?;
    }

    flush_promisor_blob_batch(&repo, &promisor, &mut batch)?;

    if sparse_mode {
        if let Ok(p) = std::env::var("GIT_TRACE2_EVENT") {
            if !p.is_empty() {
                let _ = crate::trace2_write_json_data_line(
                    &p,
                    "path-walk",
                    "paths",
                    &paths_visited.to_string(),
                );
            }
        }
    }

    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn walk_tree_for_backfill(
    repo: &Repository,
    promisor: &PromisorSource,
    tree_oid: ObjectId,
    prefix: &str,
    sparse_on: bool,
    patterns: Option<&[String]>,
    cone_mode: bool,
    marker: &mut HashSet<ObjectId>,
    use_marker_filter: bool,
    min_batch: usize,
    batch: &mut Vec<ObjectId>,
    paths_visited: &mut usize,
    path_trace_opt: &mut Option<HashSet<String>>,
    seen_trees: &mut HashSet<ObjectId>,
    queued_blobs: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !seen_trees.insert(tree_oid) {
        return Ok(());
    }
    if let Some(seen) = path_trace_opt {
        let key = if prefix.is_empty() {
            "/".to_string()
        } else {
            format!("{prefix}/")
        };
        if seen.insert(key) {
            *paths_visited += 1;
        }
    } else {
        *paths_visited += 1;
    }

    let tree_obj = match repo.odb.read(&tree_oid) {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };
    if tree_obj.kind != ObjectKind::Tree {
        return Ok(());
    }
    let entries = parse_tree(&tree_obj.data).context("parsing tree")?;

    let mut children: Vec<(String, ObjectId, u32)> = Vec::new();
    for entry in entries {
        if entry.mode == 0o160000 {
            continue;
        }
        let name = String::from_utf8_lossy(&entry.name).to_string();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let is_dir = entry.mode == 0o040000;
        if sparse_on {
            let pat_path = if is_dir {
                format!("{rel}/")
            } else {
                rel.clone()
            };
            let included = if is_dir && !cone_mode {
                true
            } else {
                crate::commands::sparse_checkout::path_matches_sparse_patterns(
                    &pat_path,
                    patterns.unwrap_or(&[]),
                    cone_mode,
                )
            };
            if !included {
                continue;
            }
        }
        children.push((rel, entry.oid, entry.mode));
    }

    children.sort_by(|a, b| {
        let a_dir = a.2 == 0o040000;
        let b_dir = b.2 == 0o040000;
        match (a_dir, b_dir) {
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        }
    });

    for (rel, oid, mode) in children {
        if mode == 0o040000 {
            walk_tree_for_backfill(
                repo,
                promisor,
                oid,
                &rel,
                sparse_on,
                patterns,
                cone_mode,
                marker,
                use_marker_filter,
                min_batch,
                batch,
                paths_visited,
                path_trace_opt,
                seen_trees,
                queued_blobs,
            )?;
            continue;
        }

        if let Some(seen) = path_trace_opt {
            if seen.insert(rel.clone()) {
                *paths_visited += 1;
            }
        } else {
            *paths_visited += 1;
        }
        if use_marker_filter && !marker.contains(&oid) {
            continue;
        }
        if repo.odb.exists_local(&oid) {
            continue;
        }
        if !queued_blobs.insert(oid) {
            continue;
        }
        batch.push(oid);
        if batch.len() >= min_batch {
            flush_promisor_blob_batch(repo, promisor, batch)?;
        }
    }

    Ok(())
}
