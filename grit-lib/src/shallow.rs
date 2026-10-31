//! Shallow repository metadata (`.git/shallow`).

use crate::error::{Error, Result};
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pkt_line;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// A sentinel `deepen` depth requesting *complete* history, used to drive an
/// `--unshallow` fetch. Git sends `INFINITE_DEPTH` (`0x7fffffff`) as the `deepen`
/// argument; the server responds by `unshallow`-ing every boundary it can fill.
pub const INFINITE_DEPTH: u32 = 0x7fff_ffff;

/// Path of the `shallow` file for `git_dir`.
///
/// The file is shared by every worktree: for a linked worktree's admin directory
/// (one with a `commondir` file) it lives in the common Git directory, as in Git.
#[must_use]
pub fn shallow_file_path(git_dir: &Path) -> PathBuf {
    crate::repo::common_git_dir_for_config(git_dir).join("shallow")
}

/// Returns the set of commit OIDs recorded as shallow boundaries in `.git/shallow`.
///
/// `git_dir` may be a linked worktree's admin directory; see [`shallow_file_path`].
///
/// For each listed commit, history must not be traversed past its parents (the parents may be
/// absent from the object database). This matches Git's behavior for `git fsck` and reachability.
#[must_use]
pub fn load_shallow_boundaries(git_dir: &Path) -> HashSet<ObjectId> {
    let shallow_path = shallow_file_path(git_dir);
    let mut set = HashSet::new();
    let Ok(contents) = fs::read_to_string(&shallow_path) else {
        return set;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(oid) = line.parse::<ObjectId>() {
            set.insert(oid);
        }
    }
    set
}

/// Drop duplicate OIDs while preserving first-seen order.
#[must_use]
pub fn dedupe_shallow_list(ids: Vec<ObjectId>) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    ids.into_iter().filter(|oid| seen.insert(*oid)).collect()
}

/// Add `new_boundaries` to `.git/shallow`, preserving any boundaries already recorded.
///
/// Mirrors `update_shallow_ref` in upstream `receive-pack`, which appends the grafts introduced by
/// a shallow push to the receiver's `.git/shallow` file. Existing boundaries are kept and the union
/// is written back sorted (Git writes the file deduplicated; ordering is not significant to readers).
///
/// # Errors
/// Returns an error if the `.git/shallow` file cannot be written.
pub fn add_shallow_boundaries(
    git_dir: &Path,
    new_boundaries: &HashSet<ObjectId>,
) -> std::io::Result<()> {
    if new_boundaries.is_empty() {
        return Ok(());
    }
    let mut all = load_shallow_boundaries(git_dir);
    all.extend(new_boundaries.iter().copied());
    let mut hexes: Vec<String> = all.iter().map(ObjectId::to_hex).collect();
    hexes.sort();
    let mut body = hexes.join("\n");
    body.push('\n');
    fs::write(shallow_file_path(git_dir), body)
}

/// The shallow boundaries a pushing client announced with `shallow <oid>` lines, classified
/// against the receiving repository.
///
/// A shallow client sends its own grafts before the ref commands. The pushed history may bottom
/// out at one of them, in which case the receiver either already has that commit's ancestry, or
/// accepting the push would make the receiver shallow there too (a *shallow update*).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PushShallowInfo {
    /// Announced commits the receiver already stored before the push and does not record as
    /// grafts of its own.
    ours: HashSet<ObjectId>,
    /// Announced commits the receiver did not have before the push.
    theirs: HashSet<ObjectId>,
}

impl PushShallowInfo {
    /// Classify `announced` against the receiver as it was before the push.
    ///
    /// Commits the receiver already records as grafts (`receiver_grafts`) are ignored: their
    /// ancestry is already cut there.
    ///
    /// # Errors
    ///
    /// Returns object database errors from the existence checks.
    pub(crate) fn classify(
        announced: &[ObjectId],
        receiver: &Odb,
        receiver_grafts: &HashSet<ObjectId>,
    ) -> Result<Self> {
        let mut info = Self::default();
        for oid in announced.iter().filter(|o| !receiver_grafts.contains(o)) {
            if receiver.contains(oid)? {
                info.ours.insert(*oid);
            } else {
                info.theirs.insert(*oid);
            }
        }
        Ok(info)
    }

    /// Drop announced commits the push did not deliver: they cannot be part of the pushed
    /// history.
    ///
    /// # Errors
    ///
    /// Returns object database errors from the existence checks.
    pub(crate) fn retain_received(&mut self, odb: &Odb) -> Result<()> {
        let mut kept = HashSet::with_capacity(self.theirs.len());
        for oid in self.theirs.drain() {
            if odb.contains(&oid)? {
                kept.insert(oid);
            }
        }
        self.theirs = kept;
        Ok(())
    }

    /// Whether no announced commit can affect the push.
    pub(crate) fn is_empty(&self) -> bool {
        self.ours.is_empty() && self.theirs.is_empty()
    }

    /// The announced commits that become new shallow roots on the receiver if `tip` is accepted.
    ///
    /// History is walked from `tip` (peeled through tags) in `odb`, which must see both the
    /// receiver's objects and the pushed ones. A line of history ends at a receiver ref tip, at
    /// one of the receiver's own grafts, or at an announced commit, which is then a candidate.
    /// A candidate the push delivered is a new root. One the receiver already had is a new root
    /// only when no receiver ref reaches it: otherwise the receiver already holds its ancestry.
    ///
    /// # Parameters
    ///
    /// - `odb`: object database holding the receiver's objects and the pushed ones.
    /// - `graph`: the receiver's commit-graph, used to bound the ancestry checks.
    /// - `tip`: the value a ref command asks for.
    /// - `receiver_grafts`: the receiver's `shallow` file entries.
    /// - `receiver_tips`: the receiver's ref values before the push, peeled to commits.
    ///
    /// # Returns
    ///
    /// The new roots, sorted; empty when accepting `tip` needs no shallow update.
    ///
    /// # Errors
    ///
    /// Returns parse errors for commits on the ancestry checks.
    pub(crate) fn new_roots(
        &self,
        odb: &Odb,
        graph: Option<&crate::commit_graph_file::CommitGraphChain>,
        tip: ObjectId,
        receiver_grafts: &HashSet<ObjectId>,
        receiver_tips: &HashSet<ObjectId>,
    ) -> Result<Vec<ObjectId>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let Some(tip) = peel_to_commit(odb, tip) else {
            return Ok(Vec::new());
        };
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![tip];
        while let Some(oid) = stack.pop() {
            if receiver_tips.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            if self.theirs.contains(&oid) {
                roots.push(oid);
                continue;
            }
            if self.ours.contains(&oid) {
                if !reached_by_any(odb, graph, oid, receiver_tips)? {
                    roots.push(oid);
                }
                continue;
            }
            if receiver_grafts.contains(&oid) {
                continue;
            }
            if let Some(parents) = commit_parents(odb, &oid) {
                stack.extend(parents);
            }
        }
        roots.sort_unstable();
        Ok(roots)
    }
}

/// Whether any of `tips` reaches `commit`.
fn reached_by_any(
    odb: &Odb,
    graph: Option<&crate::commit_graph_file::CommitGraphChain>,
    commit: ObjectId,
    tips: &HashSet<ObjectId>,
) -> Result<bool> {
    for tip in tips {
        if crate::merge_base::is_ancestor_odb_walk_with_graph(odb, graph, commit, *tip)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Follow tags from `oid` to a commit; `None` when it ends elsewhere or an object is missing.
fn peel_to_commit(odb: &Odb, mut oid: ObjectId) -> Option<ObjectId> {
    loop {
        let obj = odb.read(&oid).ok()?;
        match obj.kind {
            ObjectKind::Commit => return Some(oid),
            ObjectKind::Tag => oid = crate::objects::parse_tag(&obj.data).ok()?.object,
            _ => return None,
        }
    }
}

/// Read the shallow boundary OIDs recorded in `<git_dir>/shallow`, in file order.
///
/// Returns an empty vector when the repository is not shallow (no `shallow` file).
/// This is the ordered counterpart to [`load_shallow_boundaries`] used to build
/// the wire `shallow <oid>` lines a fetch sends so the server knows the client's
/// current grafts. Lifted from the CLI's `read_local_shallow_oids`.
///
/// # Errors
/// Returns an error only on an unexpected I/O failure reading an existing file.
pub fn load_shallow_oids(git_dir: &Path) -> Result<Vec<ObjectId>> {
    let shallow_path = shallow_file_path(git_dir);
    if !shallow_path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(&shallow_path).map_err(Error::Io)?;
    let mut out = Vec::new();
    for line in contents.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Ok(oid) = ObjectId::from_hex(line) {
            out.push(oid);
        }
    }
    Ok(out)
}

/// Apply the `shallow`/`unshallow` boundary updates a fetch's shallow-info section
/// reported to the on-disk `<git_dir>/shallow` file.
///
/// New `shallow` oids are added as boundaries; `unshallow` oids are removed (their
/// history is now complete). When the resulting set is empty the `shallow` file is
/// deleted, turning the repo back into a complete one. The file is written sorted
/// (ordering is not significant to readers). Lifted from the CLI's
/// `apply_shallow_updates`.
///
/// # Errors
/// Returns an error if the `shallow` file cannot be read or written.
pub fn apply_shallow_updates(
    git_dir: &Path,
    shallow: &[ObjectId],
    unshallow: &[ObjectId],
) -> Result<()> {
    if shallow.is_empty() && unshallow.is_empty() {
        return Ok(());
    }
    let mut boundaries: HashSet<ObjectId> = load_shallow_oids(git_dir)?.into_iter().collect();
    for oid in shallow {
        boundaries.insert(*oid);
    }
    for oid in unshallow {
        boundaries.remove(oid);
    }

    let shallow_path = shallow_file_path(git_dir);
    if boundaries.is_empty() {
        let _ = fs::remove_file(&shallow_path);
        return Ok(());
    }

    let mut lines: Vec<String> = boundaries.iter().map(ObjectId::to_hex).collect();
    lines.sort();
    let mut contents = lines.join("\n");
    contents.push('\n');
    fs::write(&shallow_path, contents).map_err(Error::Io)
}

/// Read a fetch response's `shallow-info` section, collecting `shallow <oid>` and
/// `unshallow <oid>` lines up to the terminating flush or delim.
///
/// Used by the v0/v1 streaming and v2 (streaming + stateless-HTTP) fetch paths,
/// which all precede the pack with this section when a deepen was requested. The
/// section ends at a flush (v0/v1) or a delim (v2, before the `packfile` header);
/// both stop the read, so the caller's pack reader sees the `packfile` header
/// intact. Lifted from the CLI's `read_shallow_info_section`.
///
/// # Errors
/// Returns an error on an unexpected non-data packet or a malformed oid.
/// Decode one sideband-all primary line from a data pkt-line payload.
///
/// Returns `Ok(None)` for progress packets (band 2) the caller should skip.
pub(crate) fn v2_primary_line_from_pkt(
    payload: String,
    sideband_all: bool,
) -> Result<Option<String>> {
    if !sideband_all {
        return Ok(Some(payload.trim_end().to_owned()));
    }
    let bytes = payload.as_bytes();
    if bytes.is_empty() {
        return Ok(None);
    }
    match bytes[0] {
        1 => Ok(Some(
            String::from_utf8_lossy(&bytes[1..]).trim_end().to_owned(),
        )),
        2 => Ok(None),
        3 => Err(pkt_line::sideband_fatal(&bytes[1..])),
        _ => Ok(Some(payload.trim_end().to_owned())),
    }
}

/// Read one primary payload line from a v2 fetch response (`sideband-all` demux when enabled).
pub(crate) fn read_v2_fetch_primary_line(
    r: &mut dyn Read,
    sideband_all: bool,
) -> Result<Option<String>> {
    loop {
        match pkt_line::read_packet(&mut &mut *r)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => {
                return Ok(None);
            }
            Some(pkt_line::Packet::ResponseEnd) => return Ok(None),
            Some(pkt_line::Packet::Data(payload)) => {
                if let Some(line) = v2_primary_line_from_pkt(payload, sideband_all)? {
                    return Ok(Some(line));
                }
            }
        }
    }
}

/// `read_shallow_info_section` function.
pub fn read_shallow_info_section(r: &mut dyn Read) -> Result<(Vec<ObjectId>, Vec<ObjectId>)> {
    read_shallow_info_section_sideband(r, false)
}

/// Like [`read_shallow_info_section`], demuxing `sideband-all` section lines when `sideband_all`.
pub fn read_shallow_info_section_sideband(
    r: &mut dyn Read,
    sideband_all: bool,
) -> Result<(Vec<ObjectId>, Vec<ObjectId>)> {
    let mut shallow = Vec::new();
    let mut unshallow = Vec::new();
    while let Some(line) = read_v2_fetch_primary_line(r, sideband_all)? {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("shallow ") {
            let oid = ObjectId::from_hex(rest.trim())
                .map_err(|_| Error::Message(format!("parse shallow oid {}", rest.trim())))?;
            shallow.push(oid);
        } else if let Some(rest) = line.strip_prefix("unshallow ") {
            let oid = ObjectId::from_hex(rest.trim())
                .map_err(|_| Error::Message(format!("parse unshallow oid {}", rest.trim())))?;
            unshallow.push(oid);
        }
    }
    Ok((shallow, unshallow))
}

/// Convert a `--shallow-since` / `--deepen-since` date argument into the wire
/// `deepen-since` value: a bare Unix timestamp.
///
/// Git's `fetch-pack` runs `approxidate()` on the user date and sends the integer
/// timestamp (`upload-pack` parses it with `parse_timestamp` and rejects trailing
/// garbage). Sending a raw human string makes a real `upload-pack` die silently,
/// so embedders that hand a date string here get the right wire value. A value
/// that already parses as a bare integer is passed through. Lifted from the CLI's
/// `deepen_since_wire_value`.
#[must_use]
pub fn deepen_since_wire_value(since: &str) -> String {
    let since = since.trim();
    if since.parse::<u64>().is_ok() {
        return since.to_owned();
    }
    crate::git_date::approx::approxidate_careful(since, None).to_string()
}

/// Read the parent OIDs of a commit object from `odb`, or `None` if `oid` is absent or not a commit.
fn commit_parents(odb: &Odb, oid: &ObjectId) -> Option<Vec<ObjectId>> {
    let obj = odb.read(oid).ok()?;
    if obj.kind != ObjectKind::Commit {
        return None;
    }
    let commit = parse_commit(&obj.data).ok()?;
    Some(commit.parents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_path::CanonicalizeExt;
    use tempfile::tempdir;

    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    /// Write a commit with the empty tree and the given parents, returning its OID.
    fn write_commit(odb: &Odb, parents: &[ObjectId], msg: &str) -> ObjectId {
        let mut body = format!("tree {EMPTY_TREE}\n");
        for p in parents {
            body.push_str(&format!("parent {}\n", p.to_hex()));
        }
        body.push_str("author A <a@x> 1 +0000\ncommitter A <a@x> 1 +0000\n\n");
        body.push_str(msg);
        body.push('\n');
        odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
            .expect("write commit")
    }

    fn roots(
        info: &PushShallowInfo,
        odb: &Odb,
        tip: ObjectId,
        grafts: &[ObjectId],
        tips: &[ObjectId],
    ) -> Vec<ObjectId> {
        info.new_roots(
            odb,
            None,
            tip,
            &grafts.iter().copied().collect(),
            &tips.iter().copied().collect(),
        )
        .unwrap()
    }

    #[test]
    fn no_announced_boundaries_means_no_shallow_roots() {
        let dir = tempdir().unwrap();
        let odb = Odb::new(&dir.path().join("objects"));
        let root = write_commit(&odb, &[], "root");
        let tip = write_commit(&odb, &[root], "tip");
        let info = PushShallowInfo::classify(&[], &odb, &HashSet::new()).unwrap();
        assert!(info.is_empty());
        assert!(roots(&info, &odb, tip, &[], &[]).is_empty());
    }

    #[test]
    fn delivered_boundary_is_a_new_root() {
        // Receiver is empty; the push delivers c on top of the client's boundary b.
        let receiver_dir = tempdir().unwrap();
        let receiver = Odb::new(&receiver_dir.path().join("objects"));
        let pushed_dir = tempdir().unwrap();
        let pushed = Odb::new(&pushed_dir.path().join("objects"));
        let a = write_commit(&pushed, &[], "a");
        let b = write_commit(&pushed, &[a], "b");
        let c = write_commit(&pushed, &[b], "c");
        let mut info = PushShallowInfo::classify(&[b], &receiver, &HashSet::new()).unwrap();
        info.retain_received(&pushed).unwrap();
        assert_eq!(roots(&info, &pushed, c, &[], &[]), vec![b]);
    }

    #[test]
    fn undelivered_announcement_is_dropped() {
        let dir = tempdir().unwrap();
        let odb = Odb::new(&dir.path().join("objects"));
        let mut info = PushShallowInfo::classify(&[oid(0x42)], &odb, &HashSet::new()).unwrap();
        assert!(!info.is_empty());
        info.retain_received(&odb).unwrap();
        assert!(info.is_empty());
    }

    #[test]
    fn boundary_the_receiver_already_reaches_is_not_a_new_root() {
        // The receiver has full history a <- b with `main` at b; the client is shallow at b
        // (the reported depth-1 clone) and pushes c on top.
        let dir = tempdir().unwrap();
        let odb = Odb::new(&dir.path().join("objects"));
        let a = write_commit(&odb, &[], "a");
        let b = write_commit(&odb, &[a], "b");
        let info = PushShallowInfo::classify(&[b], &odb, &HashSet::new()).unwrap();
        let c = write_commit(&odb, &[b], "c");
        assert!(roots(&info, &odb, c, &[], &[b]).is_empty());
    }

    #[test]
    fn boundary_below_a_receiver_tip_is_not_a_new_root() {
        // The receiver's `main` moved past the client's boundary `a`, and the walk from the
        // pushed tip does not pass through `main`: only the ancestry check covers `a`.
        let dir = tempdir().unwrap();
        let odb = Odb::new(&dir.path().join("objects"));
        let root = write_commit(&odb, &[], "root");
        let a = write_commit(&odb, &[root], "a");
        let main = write_commit(&odb, &[a], "main");
        let info = PushShallowInfo::classify(&[a], &odb, &HashSet::new()).unwrap();
        let pushed = write_commit(&odb, &[a], "side");
        assert!(roots(&info, &odb, pushed, &[], &[main]).is_empty());
    }

    #[test]
    fn stored_boundary_no_ref_reaches_is_a_new_root() {
        // `a` is stored on the receiver (dangling) but no ref reaches it.
        let dir = tempdir().unwrap();
        let odb = Odb::new(&dir.path().join("objects"));
        let a = write_commit(&odb, &[], "a");
        let other = write_commit(&odb, &[], "other");
        let info = PushShallowInfo::classify(&[a], &odb, &HashSet::new()).unwrap();
        let c = write_commit(&odb, &[a], "c");
        assert_eq!(roots(&info, &odb, c, &[], &[other]), vec![a]);
    }

    #[test]
    fn receiver_grafts_are_ignored_and_end_the_walk() {
        // The receiver is itself shallow at `b`; the client announces `b` and an older `a`.
        let dir = tempdir().unwrap();
        let odb = Odb::new(&dir.path().join("objects"));
        let a = write_commit(&odb, &[], "a");
        let b = write_commit(&odb, &[a], "b");
        let grafts = HashSet::from([b]);
        let info = PushShallowInfo::classify(&[a, b], &odb, &grafts).unwrap();
        let c = write_commit(&odb, &[b], "c");
        assert!(roots(&info, &odb, c, &[b], &[]).is_empty());
    }

    #[test]
    fn tags_are_peeled_before_the_walk() {
        let receiver_dir = tempdir().unwrap();
        let receiver = Odb::new(&receiver_dir.path().join("objects"));
        let dir = tempdir().unwrap();
        let odb = Odb::new(&dir.path().join("objects"));
        let a = write_commit(&odb, &[], "a");
        let b = write_commit(&odb, &[a], "b");
        let tag = odb
            .write_loose_materialize(
                ObjectKind::Tag,
                format!(
                    "object {}\ntype commit\ntag v1\ntagger A <a@x> 1 +0000\n\nv1\n",
                    b.to_hex()
                )
                .as_bytes(),
            )
            .unwrap();
        let info = PushShallowInfo::classify(&[a], &receiver, &HashSet::new()).unwrap();
        assert_eq!(roots(&info, &odb, tag, &[], &[]), vec![a]);
    }

    #[test]
    fn add_shallow_boundaries_unions_existing() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        let a = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let b = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        let mut first = HashSet::new();
        first.insert(a);
        add_shallow_boundaries(git_dir, &first).unwrap();
        let mut second = HashSet::new();
        second.insert(b);
        add_shallow_boundaries(git_dir, &second).unwrap();
        let loaded = load_shallow_boundaries(git_dir);
        assert!(loaded.contains(&a));
        assert!(loaded.contains(&b));
        assert_eq!(loaded.len(), 2);
    }

    fn oid(hex_byte: u8) -> ObjectId {
        let s = format!("{hex_byte:02x}").repeat(20);
        ObjectId::from_hex(&s).unwrap()
    }

    #[test]
    fn linked_worktree_admin_dir_uses_common_shallow_file() {
        let dir = tempdir().unwrap();
        let common = dir.path().join("repo.git");
        let admin = common.join("worktrees").join("linked");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let common = common.canonicalize_simplified().unwrap();
        assert_eq!(shallow_file_path(&admin), common.join("shallow"));
        assert_eq!(shallow_file_path(&common), common.join("shallow"));

        let a = oid(0x11);
        let b = oid(0x22);
        std::fs::write(common.join("shallow"), format!("{}\n", a.to_hex())).unwrap();
        assert_eq!(load_shallow_boundaries(&admin), HashSet::from([a]));
        assert_eq!(load_shallow_oids(&admin).unwrap(), vec![a]);

        apply_shallow_updates(&admin, &[b], &[a]).unwrap();
        assert!(!admin.join("shallow").exists());
        assert_eq!(load_shallow_boundaries(&common), HashSet::from([b]));
        apply_shallow_updates(&admin, &[], &[b]).unwrap();
        assert!(!common.join("shallow").exists());
    }

    #[test]
    fn load_shallow_oids_reads_file_in_order() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        // Missing file → empty.
        assert!(load_shallow_oids(git_dir).unwrap().is_empty());
        let a = oid(0x11);
        let b = oid(0x22);
        std::fs::write(
            git_dir.join("shallow"),
            format!("{}\n{}\n", a.to_hex(), b.to_hex()),
        )
        .unwrap();
        assert_eq!(load_shallow_oids(git_dir).unwrap(), vec![a, b]);
    }

    #[test]
    fn read_shallow_info_section_parses_shallow_and_unshallow() {
        // A v0/v1 shallow-info section: `shallow`/`unshallow` lines, flush-terminated.
        let a = oid(0xaa);
        let b = oid(0xbb);
        let mut bytes = Vec::new();
        pkt_line::write_line_to_vec(&mut bytes, &format!("shallow {}", a.to_hex())).unwrap();
        pkt_line::write_line_to_vec(&mut bytes, &format!("unshallow {}", b.to_hex())).unwrap();
        pkt_line::write_flush(&mut bytes).unwrap();
        // Trailing bytes after the flush (e.g. the start of the pack/ACK) must be
        // left unconsumed: the reader stops at the flush.
        bytes.extend_from_slice(b"PACK");

        let mut cur = std::io::Cursor::new(bytes);
        let (shallow, unshallow) = read_shallow_info_section(&mut cur).unwrap();
        assert_eq!(shallow, vec![a]);
        assert_eq!(unshallow, vec![b]);
        // The `PACK` magic remains for the caller.
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut cur, &mut rest).unwrap();
        assert_eq!(&rest, b"PACK");
    }

    #[test]
    fn v2_primary_line_from_pkt_strips_sideband_all_header() {
        let payload = format!("\u{1}packfile\n");
        let line = v2_primary_line_from_pkt(payload, true).unwrap().unwrap();
        assert_eq!(line, "packfile");
    }

    #[test]
    fn read_shallow_info_section_stops_at_delim_for_v2() {
        // A v2 `shallow-info` section is delim-terminated (before `packfile`).
        let a = oid(0xcc);
        let mut bytes = Vec::new();
        pkt_line::write_line_to_vec(&mut bytes, &format!("shallow {}", a.to_hex())).unwrap();
        pkt_line::write_delim(&mut bytes).unwrap();
        pkt_line::write_line_to_vec(&mut bytes, "packfile").unwrap();

        let mut cur = std::io::Cursor::new(bytes);
        let (shallow, unshallow) = read_shallow_info_section(&mut cur).unwrap();
        assert_eq!(shallow, vec![a]);
        assert!(unshallow.is_empty());
        // The `packfile` header is left for the caller's pack reader.
        match pkt_line::read_packet(&mut cur).unwrap() {
            Some(pkt_line::Packet::Data(s)) => assert_eq!(s.trim_end(), "packfile"),
            other => panic!("expected packfile header, got {other:?}"),
        }
    }

    #[test]
    fn apply_shallow_updates_adds_removes_and_deletes() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path();
        let a = oid(0x01);
        let b = oid(0x02);

        // Add two boundaries.
        apply_shallow_updates(git_dir, &[a, b], &[]).unwrap();
        let loaded: HashSet<ObjectId> = load_shallow_oids(git_dir).unwrap().into_iter().collect();
        assert_eq!(loaded, HashSet::from([a, b]));

        // Unshallow one: the file keeps the other.
        apply_shallow_updates(git_dir, &[], &[a]).unwrap();
        assert_eq!(load_shallow_oids(git_dir).unwrap(), vec![b]);

        // Unshallow the last boundary: the file is removed entirely.
        apply_shallow_updates(git_dir, &[], &[b]).unwrap();
        assert!(!git_dir.join("shallow").exists());
        assert!(load_shallow_oids(git_dir).unwrap().is_empty());
    }

    #[test]
    fn deepen_since_passes_through_bare_timestamp() {
        // A bare integer (what `upload-pack` expects) is passed through unchanged.
        assert_eq!(deepen_since_wire_value("200000000"), "200000000");
        assert_eq!(deepen_since_wire_value("  1234567890 "), "1234567890");
        // A human date is converted to a bare integer (no trailing garbage).
        let v = deepen_since_wire_value("2005-04-07 22:13:13 +0200");
        assert!(
            v.parse::<u64>().is_ok(),
            "expected a bare timestamp, got {v:?}"
        );
    }
}
