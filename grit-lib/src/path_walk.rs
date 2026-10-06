//! Path-batched object graph walk matching Git's `walk_objects_by_path` / `test-tool path-walk`.
//!
//! Objects are grouped by repository-relative path (trees end with `/`) and emitted in batches
//! following Git's priority queue ordering (tags, then blobs, then trees; ties by path).

use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::Path;

use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::refs;
use crate::repo::Repository;
use crate::rev_list::{
    collect_revision_specs_with_stdin, date_order_walk, resolve_object_walk_roots,
    resolve_revision_commits, walk_closure, walk_closure_ordered, CommitGraph, ObjectWalkRoot,
};
use crate::sparse_checkout::{path_in_sparse_checkout_patterns, ConePatterns};

const ROOT_PATH: &str = "";
const TAG_PATH: &str = "/tags";
const TAGGED_BLOBS_PATH: &str = "/tagged-blobs";

type PathWalkCliArgs = (PathWalkOptions, Vec<String>, Vec<String>, bool, bool);

/// Options for [`walk_objects_by_path`], aligned with Git's `struct path_walk_info`.
#[derive(Debug, Clone)]
pub struct PathWalkOptions {
    pub include_commits: bool,
    pub include_trees: bool,
    pub include_blobs: bool,
    pub include_tags: bool,
    pub prune_all_uninteresting: bool,
    pub edge_aggressive: bool,
    pub cone_patterns: Option<ConePatterns>,
    /// Lines from `test-tool path-walk --stdin-pl` (trimmed, non-empty, non-comment).
    pub sparse_pattern_lines: Option<Vec<String>>,
}

impl Default for PathWalkOptions {
    fn default() -> Self {
        Self {
            include_commits: true,
            include_trees: true,
            include_blobs: true,
            include_tags: true,
            prune_all_uninteresting: false,
            edge_aggressive: false,
            cone_patterns: None,
            sparse_pattern_lines: None,
        }
    }
}

/// One line of `test-tool path-walk` output (excluding trailing summary lines).
#[derive(Debug, Clone)]
pub struct PathWalkLine {
    pub batch: u64,
    pub object_kind: ObjectKind,
    pub path: String,
    pub oid: ObjectId,
    pub uninteresting: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PathWalkCounts {
    pub commits: u64,
    pub trees: u64,
    pub blobs: u64,
    pub tags: u64,
}

/// Run a path walk for the given revision arguments and options.
///
/// `positive_specs` and `negative_specs` are raw revision strings (no interleaved `--not`;
/// negatives are listed explicitly).
pub fn walk_objects_by_path(
    repo: &Repository,
    positive_specs: &[String],
    negative_specs: &[String],
    stdin_all: bool,
    boundary: bool,
    opts: &PathWalkOptions,
) -> Result<(Vec<PathWalkLine>, PathWalkCounts)> {
    let mut graph = CommitGraph::new(repo, false);
    let mut all_refs = stdin_all;
    let mut filtered: Vec<String> = Vec::new();
    let mut want_indexed_objects = false;
    for p in positive_specs {
        match p.as_str() {
            "--all" => all_refs = true,
            "--indexed-objects" => want_indexed_objects = true,
            "--branches" => filtered.extend(expand_branches_refs(repo)?),
            _ => filtered.push(p.clone()),
        }
    }
    let mut neg_resolved: Vec<String> = Vec::new();
    for n in negative_specs {
        match n.as_str() {
            "--all" => {
                return Err(Error::InvalidRef(
                    "--all is not valid in negative revision list".to_owned(),
                ));
            }
            "--indexed-objects" => {
                return Err(Error::InvalidRef(
                    "--indexed-objects is not valid in negative revision list".to_owned(),
                ));
            }
            "--branches" => neg_resolved.extend(expand_branches_refs(repo)?),
            _ => neg_resolved.push(n.clone()),
        }
    }
    if all_refs {
        filtered.extend(all_ref_commits(repo)?);
    }
    if filtered.is_empty() && !want_indexed_objects && !all_refs {
        return Err(Error::InvalidRef("no revisions specified".to_owned()));
    }
    let (commit_tips, mut object_roots) = if filtered.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        resolve_object_walk_roots(repo, &filtered)?
    };
    object_roots.extend(tag_object_roots_from_spec_names(repo, &filtered)?);
    if want_indexed_objects {
        object_roots.extend(indexed_blob_roots(repo)?);
    }

    let extra_tag_ref_targets: Vec<ObjectId> = if all_refs {
        tag_ref_direct_targets(repo)?
    } else {
        Vec::new()
    };
    let exclude = resolve_revision_commits(repo, &neg_resolved)?;
    let exclude_tips: HashSet<ObjectId> = exclude.iter().copied().collect();
    let excluded: HashSet<ObjectId> = if exclude.is_empty() {
        HashSet::new()
    } else {
        walk_closure(&mut graph, &exclude)?
    };

    let (included_commits, _) = if commit_tips.is_empty() {
        (HashSet::new(), Vec::new())
    } else {
        walk_closure_ordered(&mut graph, &commit_tips)?
    };
    let mut interesting_commits: HashSet<ObjectId> = included_commits
        .iter()
        .copied()
        .filter(|c| !excluded.contains(c))
        .collect();

    let boundary_commits: HashSet<ObjectId> = if boundary {
        let inc: HashSet<ObjectId> = interesting_commits.iter().copied().collect();
        let mut bset = HashSet::new();
        for &oid in &interesting_commits {
            for p in graph.parents_of(oid)? {
                if !inc.contains(&p) && excluded.contains(&p) {
                    bset.insert(p);
                }
            }
        }
        interesting_commits.extend(bset.iter().copied());
        bset
    } else {
        HashSet::new()
    };

    let mut uninteresting_commits: HashSet<ObjectId> = excluded.iter().copied().collect();
    uninteresting_commits.retain(|c| interesting_commits.contains(c));

    let excluded_objects: HashSet<ObjectId> = tree_blob_closure_from_commits(repo, &excluded)?;
    let excluded_blob_paths = excluded_blob_paths_from_commits(repo, &excluded)?;

    let mut ctx = PathWalkContext {
        repo,
        opts,
        paths: HashMap::new(),
        heap: BinaryHeap::new(),
        pushed: HashSet::new(),
        seen_object: HashSet::new(),
        uninteresting_object: HashSet::new(),
        excluded_objects,
        excluded_blob_paths,
        heap_seq: 0,
        batch: 0,
        lines: Vec::new(),
        counts: PathWalkCounts::default(),
    };

    mark_uninteresting_trees(
        repo,
        &mut graph,
        &interesting_commits,
        &excluded,
        &exclude_tips,
        opts,
        &mut ctx,
    )?;

    if opts.include_trees {
        ctx.ensure_root_list();
        ctx.push_heap(ROOT_PATH);
    }

    setup_pending_objects(repo, &object_roots, &extra_tag_ref_targets, opts, &mut ctx)?;

    // No user-supplied tip order here; equal-date ties fall through to the OID tiebreak.
    let ordered_commits = date_order_walk(&mut graph, &interesting_commits, &[], false)?;

    let mut commit_oids: Vec<ObjectId> = Vec::new();
    for c in ordered_commits {
        if !interesting_commits.contains(&c) {
            continue;
        }
        if opts.include_commits {
            commit_oids.push(c);
        }
        if !opts.include_trees && !opts.include_blobs {
            continue;
        }
        let commit = load_commit_data(repo, c)?;
        let tree_oid = commit.tree;
        if ctx.seen_object.contains(&tree_oid) {
            continue;
        }
        ctx.seen_object.insert(tree_oid);
        if ctx.excluded_objects.contains(&tree_oid) {
            ctx.uninteresting_object.insert(tree_oid);
        }
        ctx.append_root_tree(tree_oid)?;
    }

    if opts.edge_aggressive && opts.include_trees {
        for &tip in &exclude_tips {
            let commit = load_commit_data(repo, tip)?;
            let tree_oid = commit.tree;
            if ctx.seen_object.contains(&tree_oid) {
                continue;
            }
            ctx.seen_object.insert(tree_oid);
            ctx.uninteresting_object.insert(tree_oid);
            ctx.append_root_tree(tree_oid)?;
        }
    }

    if opts.include_commits && !commit_oids.is_empty() {
        ctx.emit_commit_batch(&commit_oids, &uninteresting_commits, &boundary_commits)?;
    }

    ctx.drain_heap(&mut graph)?;

    if !ctx.paths.is_empty() {
        for key in ctx.paths.keys().cloned().collect::<Vec<_>>() {
            ctx.push_heap(&key);
        }
        ctx.drain_heap(&mut graph)?;
    }

    Ok((ctx.lines, ctx.counts))
}

/// Direct `refs/tags/*` peel targets (tag object, commit, tree, or blob OID as stored in the ref).
/// When a revision token names `refs/tags/<name>` and points at an annotated tag object, include
/// that tag OID as a root (Git `setup_revisions` keeps the tag in pending).
fn tag_object_roots_from_spec_names(
    repo: &Repository,
    specs: &[String],
) -> Result<Vec<ObjectWalkRoot>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for spec in specs {
        if spec.contains("..") || spec.starts_with('^') || spec == "HEAD" {
            continue;
        }
        if spec.len() == 40 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let refname = if spec.starts_with("refs/") {
            spec.clone()
        } else {
            format!("refs/tags/{spec}")
        };
        let Ok(oid) = refs::resolve_ref(&repo.git_dir, &refname) else {
            continue;
        };
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Tag {
            continue;
        }
        if seen.insert(oid) {
            out.push(ObjectWalkRoot {
                oid,
                input: spec.clone(),
                root_path: None,
            });
        }
    }
    Ok(out)
}

fn tag_ref_direct_targets(repo: &Repository) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    for (name, _) in refs::list_refs(&repo.git_dir, "refs/tags/")? {
        let oid = refs::resolve_ref(&repo.git_dir, &name)?;
        out.push(oid);
    }
    Ok(out)
}

fn all_ref_commits(repo: &Repository) -> Result<Vec<String>> {
    let mut specs = Vec::new();
    specs.push("HEAD".to_owned());
    for (name, _) in refs::list_refs(&repo.git_dir, "refs/")? {
        specs.push(name);
    }
    Ok(specs)
}

pub fn expand_branches_refs(repo: &Repository) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (_, oid) in refs::list_refs(&repo.git_dir, "refs/heads/")? {
        out.push(oid.to_hex());
    }
    Ok(out)
}

/// Index roots for `--indexed-objects`: cache-tree root (if present), recovered subtree trees when
/// the index matches `HEAD` under a prefix, and stage-0 file blobs (each with `:path`).
fn indexed_blob_roots(repo: &Repository) -> Result<Vec<ObjectWalkRoot>> {
    let Some(_) = &repo.work_tree else {
        return Ok(Vec::new());
    };
    let index_path = repo.git_dir.join("index");
    if !index_path.is_file() {
        return Ok(Vec::new());
    }
    let idx = crate::index::Index::load(&index_path)?;
    let mut out = Vec::new();
    if let Some(root) = idx.cache_tree_root {
        out.push(ObjectWalkRoot {
            oid: root,
            input: String::new(),
            root_path: None,
        });
    }

    let head_tree = head_tree_oid(repo).ok();
    let mut file_entries: Vec<(String, ObjectId)> = Vec::new();
    for e in &idx.entries {
        if e.stage() != 0 {
            continue;
        }
        if e.mode == 0o160000 || e.mode == crate::index::MODE_TREE {
            continue;
        }
        let path_str = String::from_utf8_lossy(&e.path).into_owned();
        file_entries.push((path_str.clone(), e.oid));
        out.push(ObjectWalkRoot {
            oid: e.oid,
            input: format!(":{path_str}"),
            root_path: Some(path_str),
        });
    }

    let mut prefixes: BTreeSet<String> = BTreeSet::new();
    for (path, _) in &file_entries {
        let mut end = path.len();
        while end > 0 {
            if let Some(pos) = path[..end].rfind('/') {
                prefixes.insert(path[..pos].to_string());
                end = pos;
            } else {
                break;
            }
        }
    }

    let mut recovery_added: HashSet<String> = HashSet::new();
    let Some(ht) = head_tree else {
        return Ok(out);
    };
    for pref in prefixes {
        if pref.is_empty() {
            continue;
        }
        let mut any_under = false;
        let mut all_match = true;
        for (path, oid) in &file_entries {
            let under = if path == &pref {
                true
            } else if let Some(rest) = path.strip_prefix(&pref) {
                rest.starts_with('/')
            } else {
                false
            };
            if !under {
                continue;
            }
            any_under = true;
            match crate::rev_parse::resolve_treeish_path(repo, ht, path.as_str()) {
                Ok(h) if h == *oid => {}
                _ => {
                    all_match = false;
                    break;
                }
            }
        }
        if !any_under || !all_match {
            continue;
        }
        let tree_oid = crate::rev_parse::resolve_treeish_path(repo, ht, pref.as_str())?;
        if !recovery_added.insert(pref.clone()) {
            continue;
        }
        out.push(ObjectWalkRoot {
            oid: tree_oid,
            input: String::new(),
            root_path: Some(pref),
        });
    }

    Ok(out)
}

fn head_tree_oid(repo: &Repository) -> Result<ObjectId> {
    let head = refs::resolve_ref(&repo.git_dir, "HEAD")?;
    let obj = repo.odb.read(&head)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::InvalidRef("HEAD is not a commit".to_owned()));
    }
    let c = parse_commit(&obj.data)?;
    Ok(c.tree)
}

struct TypeOidList {
    kind: ObjectKind,
    oids: Vec<ObjectId>,
    maybe_interesting: bool,
}

struct PathWalkContext<'a> {
    repo: &'a Repository,
    opts: &'a PathWalkOptions,
    paths: HashMap<String, TypeOidList>,
    heap: BinaryHeap<std::cmp::Reverse<PathHeapItem>>,
    pushed: HashSet<String>,
    seen_object: HashSet<ObjectId>,
    uninteresting_object: HashSet<ObjectId>,
    excluded_objects: HashSet<ObjectId>,
    excluded_blob_paths: HashSet<(String, ObjectId)>,
    heap_seq: u64,
    batch: u64,
    lines: Vec<PathWalkLine>,
    counts: PathWalkCounts,
}

/// Matches Git `path-walk.c` `compare_by_type` + `prio_queue` insertion counter tie-break.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
struct PathHeapItem {
    type_rank: u8,
    path: String,
    seq: u64,
}

fn type_rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Tag => 0,
        ObjectKind::Blob => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Commit => 3,
    }
}

impl<'a> PathWalkContext<'a> {
    fn ensure_root_list(&mut self) {
        self.paths
            .entry(ROOT_PATH.to_string())
            .or_insert_with(|| TypeOidList {
                kind: ObjectKind::Tree,
                oids: Vec::new(),
                maybe_interesting: true,
            });
    }

    fn push_heap(&mut self, path: &str) {
        if !self.pushed.insert(path.to_string()) {
            return;
        }
        let Some(list) = self.paths.get(path) else {
            return;
        };
        let seq = self.heap_seq;
        self.heap_seq = self.heap_seq.wrapping_add(1);
        self.heap.push(std::cmp::Reverse(PathHeapItem {
            type_rank: type_rank(list.kind),
            path: path.to_string(),
            seq,
        }));
    }

    fn append_root_tree(&mut self, oid: ObjectId) -> Result<()> {
        self.ensure_root_list();
        let list = self
            .paths
            .get_mut(ROOT_PATH)
            .ok_or_else(|| Error::CorruptObject("root path list missing".to_owned()))?;
        list.oids.push(oid);
        self.push_heap(ROOT_PATH);
        Ok(())
    }

    fn add_path(
        &mut self,
        path: String,
        kind: ObjectKind,
        oid: ObjectId,
        interesting: bool,
    ) -> Result<()> {
        self.add_path_inner(path, kind, oid, interesting, true)
    }

    /// Record an object at `path` without enqueueing it on the path stack.
    ///
    /// Git's `setup_pending_objects` only calls `add_path_to_list` for index objects; those paths
    /// are pushed in the second `strmap_for_each_entry` phase after the main heap drain, so they
    /// do not interleave before commit root trees (`t6601` branches + indexed-objects).
    fn add_path_pending(
        &mut self,
        path: String,
        kind: ObjectKind,
        oid: ObjectId,
        interesting: bool,
    ) -> Result<()> {
        self.add_path_inner(path, kind, oid, interesting, false)
    }

    fn add_path_inner(
        &mut self,
        path: String,
        kind: ObjectKind,
        oid: ObjectId,
        interesting: bool,
        enqueue: bool,
    ) -> Result<()> {
        let list = self
            .paths
            .entry(path.clone())
            .or_insert_with(|| TypeOidList {
                kind,
                oids: Vec::new(),
                maybe_interesting: false,
            });
        if list.kind != kind {
            return Err(Error::CorruptObject(format!(
                "path-walk: inconsistent types at path {path:?}"
            )));
        }
        list.maybe_interesting |= interesting;
        list.oids.push(oid);
        if enqueue {
            self.push_heap(&path);
        }
        Ok(())
    }

    fn cone_allows_tree_path(&self, path_with_slash: &str) -> bool {
        if let Some(lines) = &self.opts.sparse_pattern_lines {
            return path_in_sparse_checkout_patterns(path_with_slash, lines, true);
        }
        if let Some(cone) = &self.opts.cone_patterns {
            let trimmed = path_with_slash.trim_end_matches('/');
            if cone.path_included(trimmed) {
                return true;
            }
            return cone.path_included(path_with_slash);
        }
        true
    }

    fn cone_allows_blob_path(&self, path: &str) -> bool {
        if let Some(lines) = &self.opts.sparse_pattern_lines {
            return path_in_sparse_checkout_patterns(path, lines, true);
        }
        if let Some(cone) = &self.opts.cone_patterns {
            return cone.path_included(path);
        }
        true
    }

    fn emit_commit_batch(
        &mut self,
        oids: &[ObjectId],
        uninteresting: &HashSet<ObjectId>,
        boundary: &HashSet<ObjectId>,
    ) -> Result<()> {
        let batch = self.batch;
        self.batch += 1;
        for &oid in oids {
            let u = uninteresting.contains(&oid) || boundary.contains(&oid);
            self.lines.push(PathWalkLine {
                batch,
                object_kind: ObjectKind::Commit,
                path: ROOT_PATH.to_string(),
                oid,
                uninteresting: u,
            });
            self.counts.commits += 1;
        }
        Ok(())
    }

    fn drain_heap(&mut self, graph: &mut CommitGraph<'_>) -> Result<()> {
        while let Some(std::cmp::Reverse(item)) = self.heap.pop() {
            self.walk_path(&item.path, graph)?;
        }
        Ok(())
    }

    fn walk_path(&mut self, path: &str, graph: &mut CommitGraph<'_>) -> Result<()> {
        let Some(mut list) = self.paths.remove(path) else {
            return Ok(());
        };
        if list.oids.is_empty() {
            return Ok(());
        }

        if self.opts.prune_all_uninteresting {
            if !list.maybe_interesting {
                return Ok(());
            }
            list.maybe_interesting = false;
            for oid in &list.oids {
                if self.object_is_interesting_at_path(path, *oid, list.kind) {
                    list.maybe_interesting = true;
                    break;
                }
            }
            if !list.maybe_interesting {
                return Ok(());
            }
        }

        let emit = match list.kind {
            ObjectKind::Tree => self.opts.include_trees,
            ObjectKind::Blob => self.opts.include_blobs,
            ObjectKind::Tag => self.opts.include_tags,
            _ => false,
        };
        let batch = if emit {
            let b = self.batch;
            self.batch += 1;
            b
        } else {
            0
        };

        match list.kind {
            ObjectKind::Tree if self.opts.include_trees => {
                for &oid in &list.oids {
                    let u = !self.tree_is_interesting(oid);
                    self.lines.push(PathWalkLine {
                        batch,
                        object_kind: ObjectKind::Tree,
                        path: path.to_string(),
                        oid,
                        uninteresting: u,
                    });
                    self.counts.trees += 1;
                }
            }
            ObjectKind::Blob if self.opts.include_blobs => {
                for &oid in &list.oids {
                    let u = !self.blob_is_interesting_at_path(path, oid);
                    self.lines.push(PathWalkLine {
                        batch,
                        object_kind: ObjectKind::Blob,
                        path: path.to_string(),
                        oid,
                        uninteresting: u,
                    });
                    self.counts.blobs += 1;
                }
            }
            ObjectKind::Tag if self.opts.include_tags => {
                for &oid in &list.oids {
                    self.lines.push(PathWalkLine {
                        batch,
                        object_kind: ObjectKind::Tag,
                        path: path.to_string(),
                        oid,
                        uninteresting: false,
                    });
                    self.counts.tags += 1;
                }
            }
            _ => {}
        }

        if list.kind == ObjectKind::Tree {
            let tree_oids = list.oids.clone();
            for tree_oid in tree_oids {
                self.add_tree_entries(path, tree_oid, graph)?;
            }
        }

        Ok(())
    }

    fn add_tree_entries(
        &mut self,
        base_path: &str,
        tree_oid: ObjectId,
        _graph: &mut CommitGraph<'_>,
    ) -> Result<()> {
        let obj = self.repo.odb.read(&tree_oid)?;
        if obj.kind != ObjectKind::Tree {
            return Err(Error::CorruptObject(format!("{tree_oid} is not a tree")));
        }
        let parent_uninteresting = !self.tree_is_interesting(tree_oid);
        let entries = parse_tree(&obj.data)?;
        for entry in entries {
            if entry.mode == 0o160000 {
                continue;
            }
            let is_tree = entry.mode == 0o040000;
            if !self.opts.include_blobs && !is_tree {
                continue;
            }
            let name = String::from_utf8_lossy(&entry.name);
            let child_oid = entry.oid;
            if self.seen_object.contains(&child_oid) {
                continue;
            }
            self.seen_object.insert(child_oid);
            if parent_uninteresting {
                self.uninteresting_object.insert(child_oid);
            }
            if is_tree {
                let rel = if base_path.is_empty() {
                    format!("{name}/")
                } else {
                    format!("{base_path}{name}/")
                };
                if !self.cone_allows_tree_path(&rel) {
                    continue;
                }
                self.add_path(
                    rel,
                    ObjectKind::Tree,
                    child_oid,
                    self.tree_is_interesting(child_oid) || !parent_uninteresting,
                )?;
            } else {
                let rel = if base_path.is_empty() {
                    name.into_owned()
                } else {
                    format!("{base_path}{}", name.as_ref())
                };
                if !self.cone_allows_blob_path(&rel) {
                    continue;
                }
                let blob_interesting =
                    self.blob_is_interesting_at_path(&rel, child_oid) || !parent_uninteresting;
                self.add_path(rel, ObjectKind::Blob, child_oid, blob_interesting)?;
            }
        }
        Ok(())
    }

    fn object_is_interesting_at_path(&self, path: &str, oid: ObjectId, kind: ObjectKind) -> bool {
        match kind {
            ObjectKind::Blob => self.blob_is_interesting_at_path(path, oid),
            ObjectKind::Tree => self.tree_is_interesting(oid),
            _ => true,
        }
    }

    fn tree_is_interesting(&self, oid: ObjectId) -> bool {
        !self.uninteresting_object.contains(&oid) && !self.excluded_objects.contains(&oid)
    }

    fn blob_is_interesting_at_path(&self, path: &str, oid: ObjectId) -> bool {
        !self.uninteresting_object.contains(&oid)
            && !self.excluded_blob_paths.contains(&(path.to_string(), oid))
    }
}

/// Every `(blob_path, blob_oid)` reachable from excluded commits' trees.
fn excluded_blob_paths_from_commits(
    repo: &Repository,
    commits: &HashSet<ObjectId>,
) -> Result<HashSet<(String, ObjectId)>> {
    let mut out = HashSet::new();
    for &c in commits {
        let commit = match load_commit_data(repo, c) {
            Ok(co) => co,
            Err(_) => continue,
        };
        collect_blob_paths(repo, commit.tree, "", &mut out)?;
    }
    Ok(out)
}

fn collect_blob_paths(
    repo: &Repository,
    tree_oid: ObjectId,
    base: &str,
    into: &mut HashSet<(String, ObjectId)>,
) -> Result<()> {
    let obj = repo.odb.read(&tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Ok(());
    }
    let entries = parse_tree(&obj.data)?;
    for e in entries {
        if e.mode == 0o160000 {
            continue;
        }
        let name = String::from_utf8_lossy(&e.name);
        if e.mode == 0o040000 {
            let next_base = if base.is_empty() {
                format!("{name}/")
            } else {
                format!("{base}{name}/")
            };
            collect_blob_paths(repo, e.oid, &next_base, into)?;
        } else {
            let path = if base.is_empty() {
                name.into_owned()
            } else {
                format!("{base}{}", name.as_ref())
            };
            into.insert((path, e.oid));
        }
    }
    Ok(())
}

fn tree_blob_closure_from_commits(
    repo: &Repository,
    commits: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    for &c in commits {
        let commit = match load_commit_data(repo, c) {
            Ok(co) => co,
            Err(_) => continue,
        };
        let mut stack = vec![commit.tree];
        while let Some(t) = stack.pop() {
            if !out.insert(t) {
                continue;
            }
            let obj = repo.odb.read(&t)?;
            if obj.kind != ObjectKind::Tree {
                continue;
            }
            let entries = parse_tree(&obj.data)?;
            for e in entries {
                if e.mode == 0o160000 {
                    continue;
                }
                out.insert(e.oid);
                if e.mode == 0o040000 {
                    stack.push(e.oid);
                }
            }
        }
    }
    Ok(out)
}

fn load_commit_data(repo: &Repository, oid: ObjectId) -> Result<crate::objects::CommitData> {
    let object = repo.odb.read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!("{oid} is not a commit")));
    }
    parse_commit(&object.data)
}

fn mark_uninteresting_trees(
    repo: &Repository,
    graph: &mut CommitGraph<'_>,
    interesting: &HashSet<ObjectId>,
    excluded: &HashSet<ObjectId>,
    exclude_tips: &HashSet<ObjectId>,
    opts: &PathWalkOptions,
    ctx: &mut PathWalkContext<'_>,
) -> Result<()> {
    if !opts.prune_all_uninteresting && !opts.edge_aggressive {
        return Ok(());
    }
    let mut queue: VecDeque<ObjectId> = interesting.iter().copied().collect();
    let mut seen_edge_commit = HashSet::new();
    while let Some(c) = queue.pop_front() {
        let parents = graph.parents_of(c)?;
        for p in parents {
            if interesting.contains(&p) {
                continue;
            }
            if !excluded.contains(&p) {
                continue;
            }
            if !seen_edge_commit.insert(p) {
                continue;
            }
            let commit = load_commit_data(repo, p)?;
            ctx.uninteresting_object.insert(commit.tree);
            queue.push_back(p);
        }
    }
    // Git `mark_edges_uninteresting`: `edge_hint_aggressive` marks trees only for commits that
    // appear on the revision cmdline as UNINTERESTING (negative tips), not the whole excluded
    // ancestry closure.
    if opts.edge_aggressive {
        for &c in exclude_tips {
            if seen_edge_commit.contains(&c) {
                continue;
            }
            let Ok(commit) = load_commit_data(repo, c) else {
                continue;
            };
            ctx.uninteresting_object.insert(commit.tree);
        }
    }
    Ok(())
}

fn setup_pending_objects(
    repo: &Repository,
    roots: &[ObjectWalkRoot],
    extra_tag_refs: &[ObjectId],
    opts: &PathWalkOptions,
    ctx: &mut PathWalkContext<'_>,
) -> Result<()> {
    let mut tag_oids: Vec<ObjectId> = Vec::new();
    let mut tagged_blob_oids: Vec<ObjectId> = Vec::new();
    let mut tag_seen: HashSet<ObjectId> = HashSet::new();

    for &oid in extra_tag_refs {
        if ctx.seen_object.contains(&oid) {
            continue;
        }
        let obj = repo.odb.read(&oid)?;
        match obj.kind {
            ObjectKind::Tag | ObjectKind::Commit => {
                if tag_seen.insert(oid) {
                    tag_oids.push(oid);
                }
            }
            ObjectKind::Tree if opts.include_trees => {
                ctx.seen_object.insert(oid);
                ctx.append_root_tree(oid)?;
            }
            ObjectKind::Blob if opts.include_blobs => {
                ctx.seen_object.insert(oid);
                tagged_blob_oids.push(oid);
            }
            _ => {}
        }
    }

    for root in roots {
        let mut oid = root.oid;
        let mut obj = repo.odb.read(&oid)?;
        while obj.kind == ObjectKind::Tag {
            if ctx.seen_object.contains(&oid) {
                break;
            }
            ctx.seen_object.insert(oid);
            if opts.include_tags && tag_seen.insert(oid) {
                tag_oids.push(oid);
            }
            let tag = parse_tag(&obj.data)?;
            oid = tag.object;
            obj = repo.odb.read(&oid)?;
        }
        if obj.kind == ObjectKind::Tag {
            continue;
        }
        if !ctx.seen_object.insert(oid) {
            continue;
        }
        match obj.kind {
            ObjectKind::Tree if opts.include_trees => {
                if let Some(p) = &root.root_path {
                    let trimmed = p.trim_end_matches('/');
                    let path = if trimmed.is_empty() {
                        "/".to_string()
                    } else {
                        format!("{trimmed}/")
                    };
                    ctx.add_path_pending(path, ObjectKind::Tree, oid, true)?;
                } else {
                    ctx.ensure_root_list();
                    ctx.paths
                        .get_mut(ROOT_PATH)
                        .ok_or_else(|| Error::CorruptObject("root path list missing".to_owned()))?
                        .oids
                        .push(oid);
                    ctx.push_heap(ROOT_PATH);
                }
            }
            ObjectKind::Blob if opts.include_blobs => {
                if let Some(p) = &root.root_path {
                    ctx.add_path_pending(p.clone(), ObjectKind::Blob, oid, true)?;
                } else {
                    tagged_blob_oids.push(oid);
                }
            }
            ObjectKind::Commit => {}
            _ => {}
        }
    }

    if opts.include_blobs && !tagged_blob_oids.is_empty() {
        let list = TypeOidList {
            kind: ObjectKind::Blob,
            oids: tagged_blob_oids,
            maybe_interesting: true,
        };
        ctx.paths.insert(TAGGED_BLOBS_PATH.to_string(), list);
        ctx.push_heap(TAGGED_BLOBS_PATH);
    }
    if opts.include_tags && !tag_oids.is_empty() {
        let list = TypeOidList {
            kind: ObjectKind::Tag,
            oids: tag_oids,
            maybe_interesting: true,
        };
        ctx.paths.insert(TAG_PATH.to_string(), list);
        ctx.push_heap(TAG_PATH);
    }
    Ok(())
}

/// Parse `test-tool path-walk` argv after the subcommand name.
///
/// Returns options, positive revision specs, negative revision specs, stdin `--all`, and `--boundary`.
pub fn parse_path_walk_cli(git_dir: &Path, args: &[String]) -> Result<PathWalkCliArgs> {
    let mut opts = PathWalkOptions::default();
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    let mut stdin_all = false;
    let mut boundary_flag = false;
    let mut after_dd = false;
    let mut not_mode = false;

    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if !after_dd && a == "--" {
            after_dd = true;
            i += 1;
            continue;
        }
        if !after_dd {
            match a.as_str() {
                "--stdin-pl" => {
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    let lines: Vec<String> = buf
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(String::from)
                        .collect();
                    if !lines.is_empty() {
                        opts.sparse_pattern_lines = Some(lines);
                    }
                }
                "--prune" => opts.prune_all_uninteresting = true,
                "--edge-aggressive" => opts.edge_aggressive = true,
                "--no-blobs" => opts.include_blobs = false,
                "--no-trees" => opts.include_trees = false,
                "--no-commits" => opts.include_commits = false,
                "--no-tags" => opts.include_tags = false,
                "--blobs" => opts.include_blobs = true,
                "--trees" => opts.include_trees = true,
                "--commits" => opts.include_commits = true,
                "--tags" => opts.include_tags = true,
                "--stdin" => {
                    let (pos, neg, all, _stdin_paths) =
                        collect_revision_specs_with_stdin(git_dir, &[], true)?;
                    stdin_all |= all;
                    positive.extend(pos);
                    negative.extend(neg);
                }
                _ => {
                    return Err(Error::InvalidRef(format!(
                        "path-walk: unknown option '{a}'"
                    )));
                }
            }
        } else if a == "--not" {
            not_mode = !not_mode;
        } else if a == "--boundary" {
            boundary_flag = true;
        } else if matches!(a.as_str(), "--all" | "--indexed-objects" | "--branches") {
            if not_mode {
                negative.push(a.clone());
            } else {
                positive.push(a.clone());
            }
        } else {
            let (p, n) = crate::rev_list::split_revision_token(a);
            if not_mode {
                negative.extend(p);
                positive.extend(n);
            } else {
                positive.extend(p);
                negative.extend(n);
            }
        }
        i += 1;
    }

    Ok((opts, positive, negative, stdin_all, boundary_flag))
}
