//! Deterministic daily-benchmark “special” repository shapes (many refs, big blobs, deep trees).

use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry, MODE_REGULAR};
use crate::objects::ObjectId;
use crate::objects::{serialize_commit, serialize_tag, CommitData, ObjectKind, TagData};
use crate::odb::Odb;
use crate::pack_write::{install_pack_stream, whole_pack_uncompressed_size, InstallPackOptions};
use crate::porcelain::checkout::checkout_between_trees;
use crate::prune_packed::{prune_packed_objects, PrunePackedOptions};
use crate::refs::{write_packed_refs_file, write_ref, PackedRefRecord};
use crate::repo::{InitOptions, Repository};
use crate::transfer::{build_pack, PackBuildOptions};
use crate::write_tree::{write_tree_from_index, write_tree_update_index, WriteTreeFlags};

use super::synth::{SynthReport, SynthTier};

/// Special daily repo shapes (see roadmap item 2354).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailySpecialShape {
    /// ~1k commits and ~10k refs in `packed-refs`.
    ManyRefs,
    /// Same history and ref names as [`Self::ManyRefs`], stored as loose ref files.
    ManyRefsLoose,
    /// ~200 paths with ~20 large binary blobs (packed).
    BigBlobs,
    /// ~5k files under 30–40 directory levels.
    DeepTree,
}

/// Where non-`HEAD` refs are stored after generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefStorage {
    Packed,
    Loose,
}

/// Resolved parameters for one special shape.
#[derive(Debug, Clone, Copy)]
struct SpecialSpec {
    tier: SynthTier,
    commit_count: usize,
    file_count: usize,
    ref_count: usize,
    edits_per_commit: usize,
    seed: u64,
    ref_storage: RefStorage,
    big_binary_count: usize,
    /// Extra large-blob revisions after the initial version (max versions = 1 + this).
    big_binary_revisions: usize,
    big_binary_min_bytes: usize,
    big_binary_max_bytes: usize,
    tree_min_depth: usize,
    tree_max_depth: usize,
    loose_after_pack: usize,
}

impl DailySpecialShape {
    /// Expected ref count after generation (`main` included).
    #[must_use]
    pub fn expected_ref_count(self, smoke: bool) -> usize {
        self.spec(smoke).ref_count
    }

    fn spec(self, smoke: bool) -> SpecialSpec {
        let full = self.full_spec();
        if smoke {
            scale_special_spec(full)
        } else {
            full
        }
    }

    fn full_spec(self) -> SpecialSpec {
        match self {
            Self::ManyRefs => SpecialSpec {
                tier: SynthTier::M,
                commit_count: 1_000,
                file_count: 200,
                ref_count: 10_000,
                edits_per_commit: 2,
                seed: 0x3122_3504,
                ref_storage: RefStorage::Packed,
                big_binary_count: 0,
                big_binary_revisions: 0,
                big_binary_min_bytes: 0,
                big_binary_max_bytes: 0,
                tree_min_depth: 0,
                tree_max_depth: 0,
                loose_after_pack: 8,
            },
            Self::ManyRefsLoose => SpecialSpec {
                ref_storage: RefStorage::Loose,
                ..Self::ManyRefs.full_spec()
            },
            Self::BigBlobs => SpecialSpec {
                tier: SynthTier::M,
                commit_count: 120,
                file_count: 200,
                ref_count: 40,
                edits_per_commit: 1,
                seed: 0x3122_3505,
                ref_storage: RefStorage::Packed,
                big_binary_count: 20,
                big_binary_revisions: 2,
                big_binary_min_bytes: 10 * 1024 * 1024,
                big_binary_max_bytes: 100 * 1024 * 1024,
                tree_min_depth: 0,
                tree_max_depth: 0,
                loose_after_pack: 0,
            },
            Self::DeepTree => SpecialSpec {
                tier: SynthTier::M,
                commit_count: 300,
                file_count: 5_000,
                ref_count: 60,
                edits_per_commit: 3,
                seed: 0x3122_3506,
                ref_storage: RefStorage::Packed,
                big_binary_count: 0,
                big_binary_revisions: 0,
                big_binary_min_bytes: 0,
                big_binary_max_bytes: 0,
                tree_min_depth: 30,
                tree_max_depth: 40,
                loose_after_pack: 4,
            },
        }
    }

    /// Inclusive directory depth range for tracked paths (leaf name excluded from depth count).
    #[must_use]
    pub fn tree_depth_range(self, smoke: bool) -> Option<(usize, usize)> {
        let spec = self.spec(smoke);
        if spec.tree_min_depth == 0 && spec.tree_max_depth == 0 {
            None
        } else {
            Some((spec.tree_min_depth, spec.tree_max_depth))
        }
    }
}

fn scale_special_spec(spec: SpecialSpec) -> SpecialSpec {
    let file_count = (spec.file_count / 20).max(10);
    let big_binary_count = if spec.big_binary_count == 0 {
        0
    } else {
        let scaled = spec.big_binary_count * file_count / spec.file_count.max(1);
        scaled.max(1).min(file_count)
    };
    let big_binary_revisions = if spec.big_binary_revisions == 0 {
        0
    } else {
        spec.big_binary_revisions.saturating_sub(1).max(1)
    };
    SpecialSpec {
        commit_count: (spec.commit_count / 20).max(10),
        file_count,
        ref_count: match spec.ref_storage {
            RefStorage::Packed | RefStorage::Loose if spec.ref_count >= 10_000 => 500,
            _ => (spec.ref_count / 20).max(5),
        },
        edits_per_commit: spec.edits_per_commit.max(1),
        big_binary_count,
        big_binary_revisions,
        big_binary_min_bytes: 1024 * 1024,
        big_binary_max_bytes: 2 * 1024 * 1024,
        loose_after_pack: if spec.tree_min_depth > 0 || big_binary_count > 0 {
            0
        } else {
            (spec.loose_after_pack / 2).max(1)
        },
        ..spec
    }
}

struct DetRng {
    state: u64,
}

impl DetRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.state
    }

    fn next_usize(&mut self, upper: usize) -> usize {
        if upper == 0 {
            return 0;
        }
        (self.next_u64() as usize) % upper
    }

    fn next_range(&mut self, min: usize, max: usize) -> usize {
        if max <= min {
            return min;
        }
        min + self.next_usize(max - min + 1)
    }
}

/// Inputs for [`plan_special_shape_edits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditPlanSpec {
    commit_count: usize,
    edits_per_commit: usize,
    file_count: usize,
    big_binary_count: usize,
    big_binary_revisions: usize,
    seed: u64,
}

impl From<SpecialSpec> for EditPlanSpec {
    fn from(spec: SpecialSpec) -> Self {
        Self {
            commit_count: spec.commit_count,
            edits_per_commit: spec.edits_per_commit,
            file_count: spec.file_count,
            big_binary_count: spec.big_binary_count,
            big_binary_revisions: spec.big_binary_revisions,
            seed: spec.seed,
        }
    }
}

/// One scheduled file rewrite (large binary or text path index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedFileEdit {
    LargeBinary(usize),
    Text(usize),
}

fn binary_rewrite_commit_map(spec: EditPlanSpec) -> std::collections::HashMap<usize, usize> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    let total_rewrites = spec.big_binary_count * spec.big_binary_revisions;
    if total_rewrites == 0 || spec.commit_count <= 1 {
        return map;
    }
    let post_initial = spec.commit_count - 1;
    for slot in 0..total_rewrites {
        let binary_idx = slot % spec.big_binary_count;
        let commit_idx = 1 + (slot * post_initial) / total_rewrites;
        map.insert(commit_idx, binary_idx);
    }
    map
}

/// Deterministic per-commit edits for `commit_idx` in `1..commit_count`.
fn plan_special_shape_edits(spec: EditPlanSpec) -> Vec<Vec<PlannedFileEdit>> {
    let rewrite_map = binary_rewrite_commit_map(spec);
    let mut rng = DetRng::new(spec.seed);
    let text_paths = spec.file_count.saturating_sub(spec.big_binary_count);
    let mut plan = Vec::with_capacity(spec.commit_count.saturating_sub(1));
    for commit_idx in 1..spec.commit_count {
        let mut row = Vec::with_capacity(spec.edits_per_commit);
        for edit in 0..spec.edits_per_commit {
            if edit == 0 {
                if let Some(&binary_idx) = rewrite_map.get(&commit_idx) {
                    row.push(PlannedFileEdit::LargeBinary(binary_idx));
                    continue;
                }
            }
            if spec.big_binary_count > 0 && text_paths > 0 {
                let fi = spec.big_binary_count + rng.next_usize(text_paths);
                row.push(PlannedFileEdit::Text(fi));
            } else {
                let fi = (commit_idx * spec.edits_per_commit + edit) % spec.file_count.max(1);
                row.push(PlannedFileEdit::Text(fi));
            }
        }
        plan.push(row);
    }
    plan
}

#[cfg(test)]
fn count_large_binary_writes(plan: &[Vec<PlannedFileEdit>], initial_count: usize) -> usize {
    let rewrites = plan
        .iter()
        .flat_map(|row| row.iter())
        .filter(|e| matches!(e, PlannedFileEdit::LargeBinary(_)))
        .count();
    initial_count + rewrites
}

#[cfg(test)]
fn per_binary_version_counts(plan: &[Vec<PlannedFileEdit>], big_binary_count: usize) -> Vec<usize> {
    let mut counts = vec![1usize; big_binary_count];
    for row in plan {
        for edit in row {
            if let PlannedFileEdit::LargeBinary(b) = edit {
                counts[*b] += 1;
            }
        }
    }
    counts
}

/// Generate a special daily-benchmark repository at `out`.
pub fn generate_daily_special_repo(
    out: &Path,
    shape: DailySpecialShape,
    smoke: bool,
) -> Result<SynthReport> {
    let spec = shape.spec(smoke);
    let started = Instant::now();
    if out.exists() {
        fs::remove_dir_all(out).map_err(Error::Io)?;
    }
    fs::create_dir_all(out).map_err(Error::Io)?;
    let repo = Repository::init(out, &InitOptions::default())?;
    let head = generate_into_repo(&repo, spec)?;
    let pack_bytes = build_pack(
        repo.odb(),
        &[head],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )?;
    let pack_uncompressed = whole_pack_uncompressed_size(&pack_bytes, repo.odb().hash_algo())?;
    install_pack_stream(
        repo.odb().objects_dir(),
        &pack_bytes,
        InstallPackOptions {
            hash_algo: repo.odb().hash_algo(),
            ..InstallPackOptions::default()
        },
    )?;
    let _ = prune_packed_objects(
        repo.odb().objects_dir(),
        PrunePackedOptions {
            quiet: true,
            ..PrunePackedOptions::default()
        },
    )?;
    if spec.loose_after_pack > 0 {
        materialize_loose_tip_objects(&repo, head, spec.loose_after_pack)?;
    }
    let head_tree = commit_tree(&repo, head)?;
    checkout_between_trees(&repo, None, &head_tree)?;
    let elapsed = started.elapsed();
    Ok(SynthReport {
        tier: spec.tier,
        head: head.to_string(),
        commit_count: spec.commit_count,
        ref_count: spec.ref_count,
        file_count: spec.file_count,
        pack_uncompressed_bytes: pack_uncompressed,
        generation_ms: elapsed.as_millis() as u64,
        repo_path: out.to_path_buf(),
    })
}

fn generate_into_repo(repo: &Repository, spec: SpecialSpec) -> Result<ObjectId> {
    let mut rng = DetRng::new(spec.seed);
    let paths = file_paths(spec);
    let mut file_oids: Vec<ObjectId> = Vec::with_capacity(paths.len());
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    let edit_plan = if spec.big_binary_count > 0 {
        Some(plan_special_shape_edits(spec.into()))
    } else {
        None
    };

    for (i, path) in paths.iter().enumerate() {
        let body = if i < spec.big_binary_count {
            binary_blob_body(
                &mut rng,
                spec.big_binary_min_bytes,
                spec.big_binary_max_bytes,
            )
        } else {
            text_blob_body(i as u32, 0, 64)
        };
        let oid = repo.odb().write(ObjectKind::Blob, &body)?;
        file_oids.push(oid);
        index.add_or_replace(synth_index_entry(path, oid, body.len()));
    }

    let mut parent: Option<ObjectId> = None;
    let mut commits: Vec<ObjectId> = Vec::with_capacity(spec.commit_count);
    for commit_idx in 0..spec.commit_count {
        if commit_idx > 0 {
            for edit in 0..spec.edits_per_commit {
                let fi = if let Some(plan) = edit_plan.as_ref() {
                    match plan[commit_idx - 1][edit] {
                        PlannedFileEdit::LargeBinary(b) => b,
                        PlannedFileEdit::Text(t) => t,
                    }
                } else {
                    (commit_idx * spec.edits_per_commit + edit) % paths.len()
                };
                let body = if fi < spec.big_binary_count {
                    binary_blob_body(
                        &mut rng,
                        spec.big_binary_min_bytes,
                        spec.big_binary_max_bytes,
                    )
                } else {
                    text_blob_body(fi as u32, commit_idx as u32, 64)
                };
                let oid = repo.odb().write(ObjectKind::Blob, &body)?;
                file_oids[fi] = oid;
                index.add_or_replace(synth_index_entry(&paths[fi], oid, body.len()));
            }
        }
        let tree = if commit_idx == 0 {
            write_tree_from_index(repo.odb(), &index, "")?
        } else {
            write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::default())?
        };
        let ts = 1_700_000_000i64 + commit_idx as i64;
        let ident = format!("Synth Generator <synth@grit-scm.com> {ts} +0000");
        let commit = CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: ident.clone(),
            committer: ident,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("special shape commit {commit_idx}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let raw = serialize_commit(&commit);
        let commit_oid = repo.odb().write(ObjectKind::Commit, &raw)?;
        commits.push(commit_oid);
        parent = Some(commit_oid);
    }
    let head = *commits
        .last()
        .ok_or_else(|| Error::CorruptObject("no commits generated".into()))?;
    let records = build_ref_records(repo, &commits, head, spec, &mut rng)?;
    install_refs(repo, &records, spec.ref_storage)?;
    Ok(head)
}

fn build_ref_records(
    repo: &Repository,
    commits: &[ObjectId],
    head: ObjectId,
    spec: SpecialSpec,
    rng: &mut DetRng,
) -> Result<Vec<PackedRefRecord>> {
    let branch_refs = spec.ref_count * 2 / 3;
    let tag_refs = spec.ref_count.saturating_sub(branch_refs).saturating_sub(1);
    let mut records = Vec::with_capacity(spec.ref_count);
    for i in 0..branch_refs {
        let name = format!("refs/heads/synth/b{i:06}");
        let oid = commits[i % commits.len()];
        records.push(PackedRefRecord {
            refname: name,
            oid,
            peeled: None,
        });
    }
    for i in 0..tag_refs {
        let commit = commits[(i * 17 + rng.next_usize(commits.len())) % commits.len()];
        if i % 2 == 0 {
            let tag_name = format!("v{i:06}");
            let ts = 1_700_100_000i64 + i as i64;
            let tagger = format!("Synth Generator <synth@grit-scm.com> {ts} +0000");
            let tag = TagData {
                object: commit,
                object_type: "commit".to_owned(),
                tag: tag_name.clone(),
                tagger: Some(tagger),
                extra_headers: Vec::new(),
                message: format!("annotated {tag_name}\n"),
            };
            let raw = serialize_tag(&tag);
            let tag_oid = repo.odb().write(ObjectKind::Tag, &raw)?;
            records.push(PackedRefRecord {
                refname: format!("refs/tags/{tag_name}"),
                oid: tag_oid,
                peeled: Some(commit),
            });
        } else {
            records.push(PackedRefRecord {
                refname: format!("refs/tags/light-{i:06}"),
                oid: commit,
                peeled: None,
            });
        }
    }
    records.push(PackedRefRecord {
        refname: "refs/heads/main".to_owned(),
        oid: head,
        peeled: None,
    });
    Ok(records)
}

fn install_refs(repo: &Repository, records: &[PackedRefRecord], storage: RefStorage) -> Result<()> {
    match storage {
        RefStorage::Packed => write_packed_refs_file(repo.git_dir(), records),
        RefStorage::Loose => {
            for record in records {
                write_ref(repo.git_dir(), &record.refname, &record.oid)?;
            }
            Ok(())
        }
    }
}

fn file_paths(spec: SpecialSpec) -> Vec<Vec<u8>> {
    if spec.tree_min_depth > 0 {
        deep_tree_paths(
            spec.file_count,
            spec.tree_min_depth,
            spec.tree_max_depth,
            spec.seed,
        )
    } else if spec.big_binary_count > 0 {
        let mut out = Vec::with_capacity(spec.file_count);
        for i in 0..spec.file_count {
            if i < spec.big_binary_count {
                out.push(format!("bin/blob_{i:03}.bin").into_bytes());
            } else {
                out.push(format!("text/doc_{i:04}.txt").into_bytes());
            }
        }
        out
    } else {
        let mut out = Vec::with_capacity(spec.file_count);
        for i in 0..spec.file_count {
            let dir = i / 20;
            out.push(format!("data/d{dir:04}/f{i:04}.txt").into_bytes());
        }
        out
    }
}

fn deep_tree_paths(count: usize, min_depth: usize, max_depth: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(count);
    let span = max_depth.saturating_sub(min_depth) + 1;
    for i in 0..count {
        let depth = min_depth + (i % span);
        let mut parts = Vec::with_capacity(depth + 1);
        let mut x = seed.wrapping_add(i as u64 * 0x9e37);
        for level in 0..depth {
            parts.push(format!(
                "s{:04x}",
                (x.wrapping_add(level as u64) & 0xffff) as u16
            ));
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        parts.push(format!("leaf_{i:05}.txt"));
        out.push(parts.join("/").into_bytes());
    }
    out
}

fn binary_blob_body(rng: &mut DetRng, min_bytes: usize, max_bytes: usize) -> Vec<u8> {
    let len = rng.next_range(min_bytes, max_bytes);
    let mut out = vec![0u8; len];
    let mut state = rng.next_u64();
    for chunk in out.chunks_mut(8) {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let bytes = state.to_le_bytes();
        for (dst, src) in chunk.iter_mut().zip(bytes.iter()) {
            *dst = *src;
        }
    }
    out
}

fn text_blob_body(file_id: u32, version: u32, body_bytes: usize) -> Vec<u8> {
    let mut s = format!("synth file={file_id} ver={version}\n");
    if s.len() < body_bytes {
        s.push_str(&"x".repeat(body_bytes - s.len()));
    }
    s.into_bytes()
}

fn materialize_loose_tip_objects(repo: &Repository, head: ObjectId, count: usize) -> Result<()> {
    let head_obj = repo.odb().read(&head)?;
    let commit = crate::objects::parse_commit(&head_obj.data)?;
    let mut blobs = Vec::new();
    collect_blob_oids(repo.odb(), &commit.tree, &mut blobs)?;
    for oid in blobs.into_iter().take(count) {
        let obj = repo.odb().read(&oid)?;
        repo.odb().write_loose_materialize(obj.kind, &obj.data)?;
    }
    Ok(())
}

fn collect_blob_oids(odb: &Odb, tree_oid: &ObjectId, out: &mut Vec<ObjectId>) -> Result<()> {
    use crate::index::MODE_TREE;
    let tree_obj = odb.read(tree_oid)?;
    for entry in crate::objects::parse_tree(&tree_obj.data)? {
        if entry.mode == MODE_TREE {
            collect_blob_oids(odb, &entry.oid, out)?;
        } else {
            out.push(entry.oid);
        }
    }
    Ok(())
}

fn commit_tree(repo: &Repository, head: ObjectId) -> Result<ObjectId> {
    let head_obj = repo.odb().read(&head)?;
    let commit = crate::objects::parse_commit(&head_obj.data)?;
    Ok(commit.tree)
}

fn synth_index_entry(path: &[u8], oid: ObjectId, size: usize) -> IndexEntry {
    IndexEntry {
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: size as u32,
        oid,
        flags: (path.len().min(0xfff)) as u16,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn path_slash_depth(path: &str) -> usize {
        path.matches('/').count() + 1
    }

    #[test]
    fn many_refs_smoke_ref_count() {
        let spec = DailySpecialShape::ManyRefs.spec(true);
        assert_eq!(spec.ref_count, 500);
    }

    #[test]
    fn deep_tree_paths_meet_depth_band() {
        let spec = DailySpecialShape::DeepTree.spec(false);
        let paths = file_paths(spec);
        let min = paths
            .iter()
            .map(|p| path_slash_depth(std::str::from_utf8(p).unwrap()))
            .min()
            .unwrap();
        let max = paths
            .iter()
            .map(|p| path_slash_depth(std::str::from_utf8(p).unwrap()))
            .max()
            .unwrap();
        assert!(min >= spec.tree_min_depth + 1);
        assert!(max <= spec.tree_max_depth + 1);
    }

    #[test]
    fn many_refs_packed_keeps_main_in_packed_refs_only() {
        let dir = TempDir::new().unwrap();
        generate_daily_special_repo(dir.path(), DailySpecialShape::ManyRefs, true).unwrap();
        let git_dir = dir.path().join(".git");
        assert!(
            !git_dir.join("refs/heads/main").exists(),
            "packed layout must not materialize loose refs/heads/main"
        );
        let packed_path = crate::refs::store::paths::packed_refs_path(&git_dir);
        assert!(packed_path.is_file(), "packed-refs file must exist");
        let packed = fs::read_to_string(packed_path).expect("packed-refs");
        assert!(
            packed
                .lines()
                .any(|line| line.ends_with(" refs/heads/main")),
            "packed-refs must list refs/heads/main"
        );
    }

    fn big_blobs_full_edit_plan_spec() -> EditPlanSpec {
        DailySpecialShape::BigBlobs.spec(false).into()
    }

    #[test]
    fn big_blobs_smoke_scales_binary_count_proportionally() {
        let smoke = DailySpecialShape::BigBlobs.spec(true);
        assert_eq!(smoke.file_count, 10);
        assert_eq!(smoke.big_binary_count, 1);
        assert_eq!(smoke.big_binary_revisions, 1);
    }

    #[test]
    fn big_blob_edit_schedule_respects_revision_cap() {
        let spec = big_blobs_full_edit_plan_spec();
        let plan = plan_special_shape_edits(spec);
        let max_versions = 1 + spec.big_binary_revisions;
        let total_large = count_large_binary_writes(&plan, spec.big_binary_count);
        assert!(
            total_large <= spec.big_binary_count * max_versions,
            "expected at most {} large blobs, got {total_large}",
            spec.big_binary_count * max_versions
        );
        for (binary, count) in per_binary_version_counts(&plan, spec.big_binary_count)
            .into_iter()
            .enumerate()
        {
            assert!(
                count <= max_versions,
                "binary {binary} has {count} versions (max {max_versions})"
            );
        }
    }

    #[test]
    fn big_blobs_head_is_deterministic() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let a =
            generate_daily_special_repo(dir_a.path(), DailySpecialShape::BigBlobs, true).unwrap();
        let b =
            generate_daily_special_repo(dir_b.path(), DailySpecialShape::BigBlobs, true).unwrap();
        assert_eq!(a.head, b.head);
    }

    #[test]
    fn packed_and_loose_many_refs_share_head() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let report_a =
            generate_daily_special_repo(dir_a.path(), DailySpecialShape::ManyRefs, true).unwrap();
        let report_b =
            generate_daily_special_repo(dir_b.path(), DailySpecialShape::ManyRefsLoose, true)
                .unwrap();
        assert_eq!(report_a.head, report_b.head);
        assert_eq!(report_a.ref_count, report_b.ref_count);
    }
}
