//! Fast object-id abbreviation using pack-index neighbors and cached loose fanout checks.

use std::collections::HashMap;
use std::path::Path;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::repo::Repository;

/// Memoized collision-aware abbreviations for a batch of known-present object ids.
#[derive(Clone)]
/// Batch abbreviator for many object ids from a revwalk (avoids per-id loose scans).
pub struct OidAbbrevBatch {
    min_len: usize,
    memo: HashMap<ObjectId, String>,
    prefix_ambiguous: HashMap<String, bool>,
    fanout_has_loose: [Option<bool>; 256],
    /// When `Some(false)`, the object directory has no loose objects (only packs).
    any_loose: Option<bool>,
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
            fanout_has_loose: [None; 256],
            any_loose: None,
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
        let abbrev = abbreviate_object_id_known(
            repo,
            oid,
            min_len,
            &mut self.prefix_ambiguous,
            &mut self.fanout_has_loose,
            &mut self.any_loose,
        )?;
        if min_len == self.min_len {
            self.memo.insert(oid, abbrev.clone());
        }
        Ok(abbrev)
    }
}

/// Like [`abbreviate_object_id_with_prefix_cache`](crate::rev_parse::abbreviate_object_id_with_prefix_cache)
/// but skips [`Odb::contains`](crate::odb::Odb::contains) and avoids full fanout scans.
pub(crate) fn abbreviate_object_id_known(
    repo: &Repository,
    oid: ObjectId,
    min_len: usize,
    prefix_ambiguous: &mut HashMap<String, bool>,
    fanout_has_loose: &mut [Option<bool>; 256],
    any_loose: &mut Option<bool>,
) -> Result<String> {
    let hex_len = oid.algo().hex_len();
    let min_len = min_len.clamp(4, hex_len);
    let mut target = oid.to_hex();

    let short = &target[..min_len];
    if prefix_ambiguous.get(short) == Some(&false) {
        target.truncate(min_len);
        return Ok(target);
    }

    let Some(mut len) = disambiguation_len_from_packs(repo, oid, min_len, hex_len)? else {
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

    let whole_odb = needs_whole_odb_disambiguation(repo)?;

    if len > min_len {
        if !whole_odb && !may_have_loose_objects(repo, any_loose)? {
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
        len = shared.map_or(len, |n| (n + 1).clamp(min_len, hex_len));
        target.truncate(len);
        return Ok(target);
    }

    if !whole_odb
        && !may_have_loose_objects(repo, any_loose)?
        && !fanout_may_have_loose(repo, oid.as_bytes()[0], fanout_has_loose)?
    {
        prefix_ambiguous.insert(short.to_owned(), false);
        target.truncate(min_len);
        return Ok(target);
    }

    let mut candidates = Vec::new();
    repo.odb().lookup_prefix(short, 2, &mut candidates)?;
    prefix_ambiguous.insert(short.to_owned(), candidates.len() > 1);
    let shared = candidates
        .iter()
        .filter(|other| **other != oid)
        .map(|other| oid.common_prefix_nibbles(other))
        .max();
    target.truncate(shared.map_or(min_len, |n| (n + 1).clamp(min_len, hex_len)));
    Ok(target)
}

fn non_promisor_pack_count(repo: &Repository) -> Result<usize> {
    Ok(repo
        .odb()
        .pack_indexes_for_display()?
        .iter()
        .filter(|idx| !idx.is_promisor)
        .count())
}

fn needs_whole_odb_disambiguation(repo: &Repository) -> Result<bool> {
    let odb = repo.odb();
    if non_promisor_pack_count(repo)? > 1 {
        return Ok(true);
    }
    if odb.has_secondary_object_stores() {
        let local = odb.local_pack_indexes()?;
        if !local.is_empty() {
            return Ok(true);
        }
        // Empty primary: pack neighbors are enough only when alternates have no loose objects.
        return Ok(odb.alternates_may_hold_loose_objects());
    }
    Ok(false)
}

fn disambiguation_len_from_packs(
    repo: &Repository,
    oid: ObjectId,
    min_len: usize,
    hex_len: usize,
) -> Result<Option<usize>> {
    let odb = repo.odb();
    let mut len = min_len;
    let indexes = odb.pack_indexes_for_display()?;
    if indexes.is_empty() {
        return Ok(None);
    }
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
        return Ok(Some(len));
    }
    Ok(None)
}

fn fanout_may_have_loose(
    repo: &Repository,
    fanout_byte: u8,
    cache: &mut [Option<bool>; 256],
) -> Result<bool> {
    let slot = fanout_byte as usize;
    if let Some(hit) = cache[slot] {
        return Ok(hit);
    }
    let has = fanout_dir_has_loose(repo.odb().objects_dir(), fanout_byte);
    cache[slot] = Some(has);
    Ok(has)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::resolve_core_abbrev_len;
    use crate::config::environment::RepositoryOptions;
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
        let mut wrote_colliding_loose = false;
        for i in 0..200_000u32 {
            let data = format!("shared-clone abbrev collision probe {i}\n");
            let blob_oid = upstream_repo
                .odb()
                .write_loose_materialize(ObjectKind::Blob, data.as_bytes())
                .expect("write loose blob");
            let blob_hex = blob_oid.to_hex();
            if blob_hex.starts_with(prefix4) && blob_hex != commit_hex {
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
        let min_len =
            resolve_core_abbrev_len(&repo.config()?.clone(), repo.odb()).expect("abbrev len");
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

fn may_have_loose_objects(repo: &Repository, cache: &mut Option<bool>) -> Result<bool> {
    if let Some(hit) = cache {
        return Ok(*hit);
    }
    let has = objects_dir_has_loose(repo.odb().objects_dir());
    *cache = Some(has);
    Ok(has)
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

fn fanout_dir_has_loose(objects_dir: &Path, fanout_byte: u8) -> bool {
    let fanout = format!("{:02x}", fanout_byte);
    let dir = objects_dir.join(&fanout);
    let Ok(mut entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.any(|e| {
        e.as_ref()
            .ok()
            .is_some_and(|ent| ent.file_type().is_ok_and(|t| t.is_file()))
    })
}
