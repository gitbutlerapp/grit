//! Fast object-id abbreviation using pack-index neighbors and cached loose fanout checks.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::odb::for_each_object_id_in_fanout_dir;
use crate::pack::PackIndex;
use crate::repo::Repository;

/// Memoized collision-aware abbreviations for a batch of known-present object ids.
#[derive(Clone)]
/// Batch abbreviator for many object ids from a revwalk (avoids per-id loose scans).
pub struct OidAbbrevBatch {
    min_len: usize,
    memo: HashMap<ObjectId, String>,
    prefix_ambiguous: HashMap<String, bool>,
    loose: LooseFanouts,
    /// Pack listing taken on first use; a batch abbreviates ids that already exist.
    packs: Option<PackListing>,
}

/// Local pack indexes and whether abbreviation must consult the whole database.
#[derive(Clone)]
pub(crate) struct PackListing {
    indexes: Vec<Arc<PackIndex>>,
    whole_odb: bool,
}

impl PackListing {
    pub fn load(repo: &Repository) -> Result<Self> {
        let odb = repo.odb();
        let indexes = odb.pack_indexes_for_display()?;
        let non_promisor = indexes.iter().filter(|idx| !idx.is_promisor).count();
        let whole_odb = if non_promisor > 1 {
            true
        } else if odb.has_secondary_object_stores() {
            !odb.local_pack_indexes()?.is_empty() || odb.alternates_may_hold_loose_objects()
        } else {
            false
        };
        Ok(Self { whole_odb, indexes })
    }
}

impl Default for OidAbbrevBatch {
    fn default() -> Self {
        Self::new(7)
    }
}

impl OidAbbrevBatch {
    /// Batch abbreviator with default `min_len` of 7 (Git `core.abbrev`).
    #[must_use]
    pub fn new(min_len: usize) -> Self {
        Self {
            min_len: min_len.clamp(4, 40),
            memo: HashMap::new(),
            prefix_ambiguous: HashMap::new(),
            loose: LooseFanouts::default(),
            packs: None,
        }
    }

    /// Minimum abbreviation length configured for this batch.
    #[must_use]
    pub(crate) fn min_len(&self) -> usize {
        self.min_len
    }

    /// Abbreviate `oid`, assuming it is present in the object database (for example from a revwalk).
    pub fn abbrev_known(&mut self, repo: &Repository, oid: ObjectId) -> Result<String> {
        self.abbrev_known_at(repo, oid, self.min_len)
    }

    /// Store a precomputed abbreviation (for example the all-zero object id).
    pub(crate) fn insert_memo(&mut self, oid: ObjectId, abbrev: String) {
        self.memo.insert(oid, abbrev);
    }

    /// Abbreviate using `min_len` instead of this batch's default.
    pub(crate) fn abbrev_known_at(
        &mut self,
        repo: &Repository,
        oid: ObjectId,
        min_len: usize,
    ) -> Result<String> {
        if min_len == self.min_len {
            if let Some(hit) = self.memo.get(&oid) {
                return Ok(hit.clone());
            }
        }
        let packs = match &mut self.packs {
            Some(packs) => packs,
            slot => slot.insert(PackListing::load(repo)?),
        };
        let abbrev = abbreviate_object_id_known(
            repo,
            packs,
            oid,
            min_len,
            &mut self.prefix_ambiguous,
            &mut self.loose,
        )?;
        if min_len == self.min_len {
            self.memo.insert(oid, abbrev.clone());
        }
        Ok(abbrev)
    }
}

/// Loose object ids per `objects/<fanout>` directory, read once per directory on demand.
#[derive(Clone, Default)]
pub(crate) struct LooseFanouts {
    /// `Some(false)` when the object directory holds no loose objects at all.
    any: Option<bool>,
    dirs: HashMap<u8, Vec<ObjectId>>,
}

impl LooseFanouts {
    /// Longest prefix (in nibbles) that `oid` shares with another loose object, if any.
    fn longest_shared_prefix(
        &mut self,
        repo: &Repository,
        oid: &ObjectId,
    ) -> Result<Option<usize>> {
        let any = match self.any {
            Some(any) => any,
            None => *self
                .any
                .insert(objects_dir_has_loose(repo.odb().objects_dir())),
        };
        if !any {
            return Ok(None);
        }
        let fanout = oid.as_bytes()[0];
        let ids = match self.dirs.entry(fanout) {
            std::collections::hash_map::Entry::Occupied(hit) => hit.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let mut ids = Vec::new();
                let hex = format!("{fanout:02x}");
                for_each_object_id_in_fanout_dir(
                    &repo.odb().objects_dir().join(&hex),
                    &hex,
                    repo.odb().hash_algo(),
                    &mut |id| {
                        ids.push(*id);
                        ControlFlow::Continue(())
                    },
                )
                .map(drop)?;
                slot.insert(ids)
            }
        };
        Ok(ids
            .iter()
            .filter(|other| *other != oid)
            .map(|other| oid.common_prefix_nibbles(other))
            .max())
    }
}

/// Like [`abbreviate_object_id_with_prefix_cache`](crate::rev_parse::abbreviate_object_id_with_prefix_cache)
/// but skips [`Odb::contains`](crate::odb::Odb::contains) and avoids full fanout scans.
///
/// With a single local pack and no other object source, the pack-index neighbours of `oid`
/// and the loose ids in its fan-out directory are the only objects that can share a prefix
/// with it, so no prefix lookup is needed.
pub(crate) fn abbreviate_object_id_known(
    repo: &Repository,
    packs: &PackListing,
    oid: ObjectId,
    min_len: usize,
    prefix_ambiguous: &mut HashMap<String, bool>,
    loose: &mut LooseFanouts,
) -> Result<String> {
    let hex_len = oid.algo().hex_len();
    let min_len = min_len.clamp(4, hex_len);
    let mut target = oid.to_hex();

    let short = &target[..min_len];
    if prefix_ambiguous.get(short) == Some(&false) {
        target.truncate(min_len);
        return Ok(target);
    }

    let Some(mut len) = disambiguation_len_from_packs(&packs.indexes, oid, min_len, hex_len) else {
        let mut candidates = Vec::new();
        repo.odb().lookup_prefix(short, 0, &mut candidates)?;
        prefix_ambiguous.insert(short.to_owned(), candidates.len() > 1);
        let shared = candidates
            .iter()
            .filter(|other| **other != oid)
            .map(|other| oid.common_prefix_nibbles(other))
            .max();
        target.truncate(shared.map_or(min_len, |n| (n + 1).clamp(min_len, hex_len)));
        return Ok(target);
    };

    if !packs.whole_odb {
        if let Some(shared) = loose.longest_shared_prefix(repo, &oid)? {
            len = len.max((shared + 1).min(hex_len));
        }
        target.truncate(len);
        return Ok(target);
    }

    let prefix = &target[..len];
    if prefix_ambiguous.get(prefix) == Some(&false) {
        target.truncate(len);
        return Ok(target);
    }
    let mut candidates = Vec::new();
    repo.odb().lookup_prefix(prefix, 2, &mut candidates)?;
    prefix_ambiguous.insert(prefix.to_owned(), candidates.len() > 1);
    let shared = candidates
        .iter()
        .filter(|other| **other != oid)
        .map(|other| oid.common_prefix_nibbles(other))
        .max();
    len = shared.map_or(len, |n| (n + 1).clamp(len, hex_len));
    target.truncate(len);
    Ok(target)
}

fn disambiguation_len_from_packs(
    indexes: &[Arc<PackIndex>],
    oid: ObjectId,
    min_len: usize,
    hex_len: usize,
) -> Option<usize> {
    let mut len = min_len;
    for idx in indexes {
        if idx.is_promisor {
            continue;
        }
        let Some(pos) = idx.find_position(&oid) else {
            continue;
        };
        if pos > 0 {
            if let Ok(other) = ObjectId::from_bytes(idx.oid_at(pos - 1)) {
                len = len.max((oid.common_prefix_nibbles(&other) + 1).min(hex_len));
            }
        }
        if pos + 1 < idx.len() {
            if let Ok(other) = ObjectId::from_bytes(idx.oid_at(pos + 1)) {
                len = len.max((oid.common_prefix_nibbles(&other) + 1).min(hex_len));
            }
        }
        return Some(len);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::resolve_core_abbrev_len;
    use crate::config::environment::RepositoryOptions;
    use crate::hash::hash_object;
    use crate::objects::{ObjectId, ObjectKind};
    use crate::repo::Repository;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = git_output(dir, args);
        assert!(out.status.success(), "git {args:?}");
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let out = git_output(dir, args);
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git")
    }

    #[test]
    fn abbrev_known_packed_history_avoids_exists_probes() -> Result<()> {
        let root = TempDir::new().expect("tempdir");
        let dir = root.path();
        git(dir, &["init", "-q", "-b", "main"]);
        let import = r#"python3 - <<'PY' | git fast-import --quiet
import random
random.seed(2)
t=1500000000
prev=None
for i in range(1,401):
    t+=30
    msg='change %d'%i
    print('commit refs/heads/main\nmark :%d\nauthor A <a@e.t> %d +0000\ncommitter A <a@e.t> %d +0000\ndata %d\n%s'%(i,t,t,len(msg),msg))
    if prev: print('from :%d'%prev)
    b='line\n'*5
    print('M 100644 inline f%d.txt\ndata %d\n%s'%(i,len(b),b))
    prev=i
PY"#;
        let out = Command::new("bash")
            .current_dir(dir)
            .args(["-c", import])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("import");
        assert!(
            out.status.success(),
            "import: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        git(dir, &["gc", "--quiet", "--aggressive"]);

        let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open");
        let mut commits = Vec::new();
        repo.odb().for_each_object(&mut |oid| {
            if repo
                .odb()
                .read(oid)
                .ok()
                .is_some_and(|o| o.kind == ObjectKind::Commit)
            {
                commits.push(*oid);
            }
            std::ops::ControlFlow::Continue(())
        })?;
        repo.odb().reset_exists_probe();
        let mut batch = OidAbbrevBatch::new(7);
        for oid in commits {
            batch.abbrev_known(&repo, oid)?;
        }
        assert_eq!(repo.odb().exists_probe_count(), 0);
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn loose_objects_sharing_a_packed_commit_prefix_lengthen_its_abbrev_like_git() -> Result<()> {
        let root = TempDir::new().expect("tempdir");
        let dir = root.path();
        git(dir, &["init", "-q", "-b", "main"]);
        for n in 0..6 {
            std::fs::write(dir.join("f.txt"), format!("{n}\n")).expect("write");
            git(dir, &["add", "f.txt"]);
            git(
                dir,
                &[
                    "-c",
                    "user.name=A",
                    "-c",
                    "user.email=a@e.t",
                    "commit",
                    "-q",
                    "-m",
                    &format!("c{n}"),
                ],
            );
        }
        git(dir, &["gc", "--quiet"]);

        let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open");
        let mut commits = Vec::new();
        repo.odb().for_each_object(&mut |oid| {
            if repo
                .odb()
                .read(oid)
                .is_ok_and(|o| o.kind == ObjectKind::Commit)
            {
                commits.push(*oid);
            }
            std::ops::ControlFlow::Continue(())
        })?;
        assert_eq!(commits.len(), 6);
        // Loose blobs sharing the first two bytes with three of the packed commits.
        let targets: Vec<[u8; 2]> = commits
            .iter()
            .take(3)
            .map(|c| [c.as_bytes()[0], c.as_bytes()[1]])
            .collect();
        let mut found = vec![false; targets.len()];
        for n in 0_u64.. {
            let data = format!("loose {n}\n").into_bytes();
            let oid = crate::hash::hash_object(repo.odb().hash_algo(), ObjectKind::Blob, &data)?;
            if let Some(i) = targets.iter().position(|t| oid.as_bytes()[..2] == t[..]) {
                if !found[i] {
                    repo.odb().write(ObjectKind::Blob, &data)?;
                    found[i] = true;
                }
            }
            if found.iter().all(|f| *f) {
                break;
            }
        }

        let mut batch = OidAbbrevBatch::new(4);
        for oid in &commits {
            let git_short = Command::new("git")
                .current_dir(dir)
                .args(["rev-parse", "--short=4", &oid.to_hex()])
                .output()
                .expect("git");
            let git_short = String::from_utf8_lossy(&git_short.stdout).trim().to_owned();
            assert_eq!(batch.abbrev_known(&repo, *oid)?, git_short, "{oid}");
        }
        for oid in commits.iter().take(3) {
            assert!(batch.abbrev_known(&repo, *oid)?.len() > 4);
        }
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn pack_listing_outside_read_context_reuses_the_mapped_index() -> Result<()> {
        let root = TempDir::new().expect("tempdir");
        let dir = root.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("f.txt"), "one\n").expect("write");
        git(dir, &["add", "f.txt"]);
        git(
            dir,
            &[
                "-c",
                "user.name=A",
                "-c",
                "user.email=a@e.t",
                "commit",
                "-q",
                "-m",
                "one",
            ],
        );
        git(dir, &["gc", "--quiet"]);

        let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open");
        let odb = repo.odb();
        // A log walk reads under the pack-read context; abbreviating its commits afterwards
        // must see the same index, not map the file a second time.
        let walked = odb.with_pack_read_context(|| odb.local_pack_indexes())?;
        let abbreviated = odb.local_pack_indexes()?;
        assert_eq!(walked.len(), 1);
        assert_eq!(abbreviated.len(), 1);
        assert!(std::sync::Arc::ptr_eq(&walked[0], &abbreviated[0]));
        Ok(())
    }

    /// Borrow-only shared clone with a loose alternate object sharing a short prefix must
    /// extend abbreviations like Git, not stop at pack neighbors alone.
    #[test]
    fn shared_clone_abbrev_extends_when_alternate_has_loose_prefix_collision() -> Result<()> {
        let upstream = TempDir::new().expect("tempdir");
        let up = upstream.path();
        git(up, &["init", "-q", "-b", "main"]);
        std::fs::write(up.join("seed.txt"), b"seed\n").expect("write");
        git(up, &["add", "seed.txt"]);
        git(up, &["commit", "-qm", "seed"]);
        git(up, &["gc", "--quiet"]);
        let commit_hex = git_stdout(up, &["rev-parse", "HEAD"]);
        let commit_oid = ObjectId::from_hex(&commit_hex).expect("commit oid");
        let prefix4 = &commit_hex[..4];

        let upstream_repo =
            Repository::discover(Some(up), &RepositoryOptions::empty()).expect("open upstream");
        let hash_algo = upstream_repo.odb().hash_algo();
        let mut wrote_colliding_loose = false;
        // Hash cheap blobs first: expect ~one 4-nibble prefix match every 65,536 tries; only
        // persist a matching blob (avoids flaky 200k caps from disk writes + duplicate hits).
        const MAX_PROBE: u64 = 30_000_000;
        for i in 0..MAX_PROBE {
            let data = format!("shared-clone abbrev collision probe {i}\n");
            let blob_oid =
                hash_object(hash_algo, ObjectKind::Blob, data.as_bytes()).expect("hash loose blob");
            let blob_hex = blob_oid.to_hex();
            if blob_hex.starts_with(prefix4) && blob_hex != commit_hex {
                upstream_repo
                    .odb()
                    .write_loose_materialize(ObjectKind::Blob, data.as_bytes())
                    .expect("write loose blob");
                wrote_colliding_loose = true;
                break;
            }
        }
        assert!(
            wrote_colliding_loose,
            "could not synthesize a loose blob sharing the commit's 4-hex prefix"
        );

        let clone_root = TempDir::new().expect("clone tempdir");
        let clone = clone_root.path();
        let status = Command::new("git")
            .args([
                "clone",
                "--shared",
                &up.to_string_lossy(),
                &clone.to_string_lossy(),
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("clone");
        assert!(status.success(), "git clone --shared failed");
        git(clone, &["config", "core.abbrev", "4"]);

        let expected = git_stdout(clone, &["log", "--oneline", "-n", "1"]);
        let expected_abbrev = expected.split_whitespace().next().expect("git log abbrev");

        let repo = Repository::discover(Some(clone), &RepositoryOptions::empty()).expect("open");
        let min_len = resolve_core_abbrev_len(&repo.config().snapshot()?.clone(), repo.odb())
            .expect("abbrev len");
        assert_eq!(min_len, 4, "fixture must use core.abbrev=4");
        let grit_abbrev = OidAbbrevBatch::new(min_len).abbrev_known(&repo, commit_oid)?;
        assert_eq!(
            grit_abbrev, expected_abbrev,
            "grit abbrev must match git when alternate loose shares the short prefix \
             (git line {expected:?})"
        );
        assert!(
            grit_abbrev.len() > 4,
            "abbrev must extend past core.abbrev when a loose alternate object collides"
        );
        Ok(())
    }
}

fn objects_dir_has_loose(objects_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(objects_dir) else {
        return false;
    };
    for ent in entries.flatten() {
        let name = ent.file_name();
        let s = name.to_string_lossy();
        if s.len() != 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(sub) = std::fs::read_dir(ent.path()) else {
            continue;
        };
        if sub.flatten().any(|e| e.path().is_file()) {
            return true;
        }
    }
    false
}
