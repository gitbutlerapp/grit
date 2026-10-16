//! Deterministic synthetic repository generator (grit-lib backed).

use std::fs;
use std::path::{Path, PathBuf};
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

/// Benchmark fixture size tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthTier {
    /// CI-sized repo (~2k files, ~500 commits, ~1k refs).
    S,
    /// Medium repo (~20k files, ~5k commits, ~10k refs).
    M,
    /// Large repo (≥100k files, ≥50k commits, ≥100k refs, ≥1 GiB pack uncompressed).
    L,
}

/// Resolved numeric targets for a tier.
#[derive(Debug, Clone, Copy)]
pub struct TierSpec {
    /// Tier label these numbers came from.
    pub tier: SynthTier,
    /// Number of tracked file paths in the initial index.
    pub file_count: usize,
    /// Linear commit count on `refs/heads/main`.
    pub commit_count: usize,
    /// Total refs (branches, tags, plus `main`).
    pub ref_count: usize,
    /// Blob rewrites per commit after the root commit.
    pub edits_per_commit: usize,
    /// Target size for generated blob bodies (bytes).
    pub blob_body_bytes: usize,
    /// Loose blobs materialized at the tip after pack install.
    pub loose_after_pack: usize,
    /// Deterministic RNG seed for ref and edit selection.
    pub seed: u64,
}

impl SynthTier {
    /// Parse a tier name (`S`, `M`, or `L`, any ASCII case).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRef`] when `name` is not a known tier.
    pub fn parse_name(name: &str) -> Result<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "S" => Ok(Self::S),
            "M" => Ok(Self::M),
            "L" => Ok(Self::L),
            _ => Err(Error::InvalidRef(format!(
                "unknown fixture synth tier: {name}"
            ))),
        }
    }

    /// Fixed tier parameters used by tests and the CLI.
    #[must_use]
    pub fn spec(self) -> TierSpec {
        match self {
            SynthTier::S => TierSpec {
                tier: self,
                file_count: 2_000,
                commit_count: 500,
                ref_count: 1_000,
                edits_per_commit: 4,
                blob_body_bytes: 48,
                loose_after_pack: 8,
                seed: 0x312_0001,
            },
            SynthTier::M => TierSpec {
                tier: self,
                file_count: 20_000,
                commit_count: 5_000,
                ref_count: 10_000,
                edits_per_commit: 8,
                blob_body_bytes: 64,
                loose_after_pack: 16,
                seed: 0x312_0002,
            },
            SynthTier::L => TierSpec {
                tier: self,
                file_count: 100_000,
                commit_count: 50_000,
                ref_count: 100_000,
                edits_per_commit: 4,
                blob_body_bytes: 12_000,
                loose_after_pack: 32,
                seed: 0x312_0003,
            },
        }
    }
}

/// Summary returned after generation completes.
#[derive(Debug, Clone)]
pub struct SynthReport {
    /// Tier that was generated.
    pub tier: SynthTier,
    /// Hex object id of `HEAD`.
    pub head: String,
    /// Commits on `main` (matches [`TierSpec::commit_count`]).
    pub commit_count: usize,
    /// Ref count (matches [`TierSpec::ref_count`]).
    pub ref_count: usize,
    /// Tracked paths (matches [`TierSpec::file_count`]).
    pub file_count: usize,
    /// Sum of inflated object sizes in the installed pack.
    pub pack_uncompressed_bytes: u64,
    /// Wall time spent generating the repository (milliseconds).
    pub generation_ms: u64,
    /// Output directory passed to [`generate_synth_repo`].
    pub repo_path: PathBuf,
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
}

/// Generate a deterministic synthetic repository at `out`.
pub fn generate_synth_repo(out: &Path, tier: SynthTier) -> Result<SynthReport> {
    let spec = tier.spec();
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
    materialize_loose_tip_objects(&repo, head, spec.loose_after_pack)?;
    let head_tree = commit_tree(&repo, head)?;
    checkout_between_trees(&repo, None, &head_tree)?;
    let elapsed = started.elapsed();
    Ok(SynthReport {
        tier,
        head: head.to_string(),
        commit_count: spec.commit_count,
        ref_count: spec.ref_count,
        file_count: spec.file_count,
        pack_uncompressed_bytes: pack_uncompressed,
        generation_ms: elapsed.as_millis() as u64,
        repo_path: out.to_path_buf(),
    })
}

fn generate_into_repo(repo: &Repository, spec: TierSpec) -> Result<ObjectId> {
    let mut rng = DetRng::new(spec.seed);
    let paths = file_paths(spec.tier, spec.file_count);
    let mut file_oids: Vec<ObjectId> = Vec::with_capacity(paths.len());
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();

    for (i, path) in paths.iter().enumerate() {
        let body = blob_body(spec.tier, i as u32, 0, spec.blob_body_bytes);
        let oid = repo.odb().write(ObjectKind::Blob, &body)?;
        file_oids.push(oid);
        index.add_or_replace(synth_index_entry(path, oid, body.len()));
    }

    let mut parent: Option<ObjectId> = None;
    let mut commits: Vec<ObjectId> = Vec::with_capacity(spec.commit_count);
    for commit_idx in 0..spec.commit_count {
        if commit_idx > 0 {
            for _ in 0..spec.edits_per_commit {
                let fi = rng.next_usize(paths.len());
                let body = blob_body(
                    spec.tier,
                    fi as u32,
                    commit_idx as u32,
                    spec.blob_body_bytes,
                );
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
            message: format!("synthetic commit {commit_idx}\n"),
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
    write_refs(repo, &commits, spec, &mut rng)?;
    write_ref(repo.git_dir(), "refs/heads/main", &head)?;
    Ok(head)
}

fn write_refs(
    repo: &Repository,
    commits: &[ObjectId],
    spec: TierSpec,
    rng: &mut DetRng,
) -> Result<()> {
    let branch_refs = spec.ref_count * 2 / 3;
    let tag_refs = spec.ref_count - branch_refs - 1; // minus main written loose
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
    write_packed_refs_file(repo.git_dir(), &records)?;
    Ok(())
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

fn file_paths(tier: SynthTier, count: usize) -> Vec<Vec<u8>> {
    match tier {
        SynthTier::S | SynthTier::M => {
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let dir = i / 100;
                let file = i % 100;
                out.push(format!("data/d{dir:04}/f{file:04}.txt").into_bytes());
            }
            out
        }
        SynthTier::L => {
            let wide_count = count * 2 / 3;
            let mut out = Vec::with_capacity(count);
            for i in 0..wide_count {
                let dir = i / 1000;
                let file = i % 1000;
                out.push(format!("wide/w{dir:05}/f{file:04}.txt").into_bytes());
            }
            for j in 0..(count - wide_count) {
                let i = wide_count + j;
                let seg = |k: usize| format!("{:02x}", (i.wrapping_mul(0x9e37) >> (k * 4)) & 0xff);
                out.push(
                    format!(
                        "deep/{}/{}/{}/{}/{}/{}/{}/{}/leaf_{j:05}.txt",
                        seg(0),
                        seg(1),
                        seg(2),
                        seg(3),
                        seg(4),
                        seg(5),
                        seg(6),
                        seg(7),
                    )
                    .into_bytes(),
                );
            }
            out
        }
    }
}

fn blob_body(tier: SynthTier, file_id: u32, version: u32, body_bytes: usize) -> Vec<u8> {
    let mut s = match tier {
        SynthTier::L => {
            let line_count = 32usize;
            let mut lines: Vec<String> = (0..line_count)
                .map(|ln| format!("file={file_id} line={ln} seed=0"))
                .collect();
            for churn in 0..3usize {
                let ln = ((file_id as usize + version as usize + churn) % (line_count - 2)) + 1;
                lines[ln] = format!("file={file_id} line={ln} ver={version} churn={churn}");
            }
            let mut joined = lines.join("\n");
            joined.push('\n');
            joined
        }
        SynthTier::S | SynthTier::M => format!("synth file={file_id} ver={version}\n"),
    };
    if s.len() < body_bytes {
        s.push_str(&"x".repeat(body_bytes - s.len()));
    }
    s.into_bytes()
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn path_component_depth(path: &[u8]) -> usize {
        path.iter().filter(|&&b| b == b'/').count() + 1
    }

    #[test]
    fn l_tier_paths_are_deep_and_wide() {
        let paths = file_paths(SynthTier::L, 10_000);
        let max_depth = paths
            .iter()
            .map(|p| path_component_depth(p))
            .max()
            .unwrap_or(0);
        assert!(
            max_depth >= 10,
            "expected deep hierarchy, max depth was {max_depth}"
        );
        let wide_shallow = paths
            .iter()
            .filter(|p| path_component_depth(p) <= 3)
            .count();
        assert!(
            wide_shallow > 5_000,
            "expected wide shallow files, got {wide_shallow}"
        );
    }

    #[test]
    fn l_tier_blobs_have_multiline_churn() {
        let body = blob_body(SynthTier::L, 7, 42, 800);
        let text = String::from_utf8(body).expect("utf-8");
        assert!(text.lines().count() >= 20, "expected many lines for blame");
        assert!(text.contains("ver=42"));
        assert!(text.contains("churn=0"));
    }

    #[test]
    fn s_tier_keeps_shallow_layout() {
        let paths = file_paths(SynthTier::S, 100);
        assert!(paths.iter().all(|p| path_component_depth(p) == 3));
    }
}
