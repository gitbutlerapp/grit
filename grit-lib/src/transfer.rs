//! Embedder-facing transfer (fetch / push) result & option types, plus the
//! negotiation-driven pack builder.
//!
//! This module is the foundation for the in-process fetch/push APIs that
//! embedders such as `jj` and GitButler consume in place of `gix` transport.
//! It defines the structured input/output types those APIs use and implements
//! the single most important primitive — [`build_pack`] — which packs **only**
//! the objects reachable from a negotiated set of `wants` and not already
//! reachable from the remote's `haves`.
//!
//! Scope note (phase 1): only the local / `file://` object+ref copy path is in
//! scope. `git://`, `http(s)`, and `ssh` transports plus credential-helper
//! execution are out of scope and are left as TODOs in later phases.
//!
//! Push *result* reporting reuses [`crate::push_report::PushRefResult`] /
//! [`crate::push_report::PushRefStatus`] rather than redefining it.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::error::{Error, Result};
use crate::hash;
use crate::objects::{parse_tag, HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::refspec::{parse_fetch_refspec, RefspecItem};

/// How a single reference resolved during a fetch (or would resolve in a push).
///
/// Mirrors the shapes of `gix::remote::fetch::refs::update::Mode` that `jj`
/// already consumes, so the embedder's translation layer stays a thin adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateMode {
    /// The local tracking ref did not exist and was created.
    New,
    /// The update advanced the ref along its existing history.
    FastForward,
    /// A non-fast-forward update that was applied because force was requested.
    Forced,
    /// The local ref already matched the remote value; nothing to do.
    UpToDate,
    /// No change was required (e.g. a no-op refspec).
    NoChangeNeeded,
    /// A non-fast-forward update that was rejected (force not requested).
    NonFastForwardRejected,
    /// A tag update was rejected (tags are not overwritten without force).
    TagUpdateRejected,
    /// The source object named by the refspec was not found on the remote.
    SourceObjectNotFound,
    /// The remote ref is unborn (points at nothing yet).
    Unborn,
    /// A prune/delete was requested but the local ref was already missing.
    DeletedMissing,
}

/// The resolved outcome of one reference during a fetch.
#[derive(Clone, Debug)]
pub struct RefUpdate {
    /// The remote-side ref name (e.g. `refs/heads/main`).
    pub remote_ref: String,
    /// The local-side ref name written, if any (e.g. `refs/remotes/origin/main`).
    pub local_ref: Option<String>,
    /// Previous value of the local ref (`None` when newly created).
    pub old_oid: Option<ObjectId>,
    /// New value written to the local ref (`None` for deletions / unborn).
    pub new_oid: Option<ObjectId>,
    /// How the update resolved.
    pub mode: UpdateMode,
    /// Optional human-readable note (reason text), for embedder display.
    pub note: Option<String>,
}

/// Which tags to fetch alongside the requested refs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TagMode {
    /// Do not fetch any tags automatically.
    None,
    /// Fetch tags that point at objects being fetched (Git's default).
    #[default]
    Following,
    /// Fetch all tags from the remote.
    All,
}

/// Options controlling a fetch.
#[derive(Clone, Default)]
pub struct FetchOptions {
    /// Positive refspecs selecting what to fetch.
    pub refspecs: Vec<String>,
    /// Negative refspecs excluding refs from the positive set.
    pub negative_refspecs: Vec<String>,
    /// Tag-following policy.
    pub tags: TagMode,
    /// Whether to prune local tracking refs that vanished on the remote.
    pub prune: bool,
    /// Compute and report updates without writing any refs or objects.
    pub dry_run: bool,
    /// Truncate history to the given number of commits per tip
    /// (`git fetch --depth N`). Drives the wire `deepen N` / v2 `deepen` arg and,
    /// for a previously shallow repo, deepens the existing boundary. `None`
    /// requests full history.
    pub depth: Option<u32>,
    /// Deepen history to include commits no older than this cutoff
    /// (`git fetch --shallow-since <date>`). The value is sent verbatim as the
    /// wire `deepen-since <value>`; callers should pass the Unix timestamp Git's
    /// `upload-pack` expects (a bare integer), not a human date string.
    pub deepen_since: Option<String>,
    /// Deepen history but stop at (exclude) commits reachable from these refs/oids
    /// (`git fetch --shallow-exclude <ref>`). Each entry is sent as a wire
    /// `deepen-not <ref>`.
    pub deepen_not: Vec<String>,
    /// Convert a shallow repository back into a complete one
    /// (`git fetch --unshallow`). Drives the wire `deepen 0x7fffffff` request and
    /// removes the local `shallow` boundaries that get reported as `unshallow`.
    pub unshallow: bool,
    /// First fetch populating a remote-tracking namespace (clone): write `packed-refs`
    /// and `refs/remotes/{remote}/HEAD`.
    pub initial_remote_fetch: bool,
    /// Remote name paired with [`Self::initial_remote_fetch`] (e.g. `"origin"`).
    pub remote_name: Option<String>,
    /// Clone-only reflog entries for `refs/remotes/{remote}/HEAD` (not remote branches).
    pub clone_reflog: Option<CloneReflog>,
    /// Diagnostic sink for [`crate::diagnostics::Trace::Network`] events.
    pub diagnostics: Option<crate::diagnostics::DiagnosticsHandle>,
    /// When true with [`Self::diagnostics`], fetch paths may emit network trace events.
    pub network_trace: bool,
}

/// Identity and message for clone reflog entries written by the library.
#[derive(Clone, Debug)]
pub struct CloneReflog {
    /// `"Name <email> <unix_time> <tz>"` formatted identity.
    pub identity: String,
    /// Reflog message (e.g. `clone: from <url>`).
    pub message: String,
}

impl FetchOptions {
    /// Whether this fetch carries any shallow/deepen request (an explicit
    /// `depth`/`deepen-since`/`deepen-not`/`unshallow`). Note this does NOT cover
    /// the "already shallow, fetching more of the same boundary" case — that is
    /// driven by the on-disk `shallow` file, checked separately by the fetch
    /// paths via [`crate::shallow::load_shallow_oids`].
    #[must_use]
    pub fn has_deepen_request(&self) -> bool {
        self.depth.is_some()
            || self
                .deepen_since
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
            || self.deepen_not.iter().any(|v| !v.trim().is_empty())
            || self.unshallow
    }
}

/// The structured result of a fetch, ready for the embedder's ref-store apply.
#[derive(Clone, Debug, Default)]
pub struct FetchOutcome {
    /// Per-ref resolved updates.
    pub updates: Vec<RefUpdate>,
    /// The remote's default branch (from `HEAD` symref), if known.
    pub default_branch: Option<String>,
    /// New shallow boundary commits the server reported (`shallow <oid>`), already
    /// applied to the local `shallow` file. The commits' parents are intentionally
    /// absent from the local object store after this fetch.
    pub new_shallow: Vec<ObjectId>,
    /// Commits the server reported as no longer shallow (`unshallow <oid>`), i.e.
    /// boundaries removed from the local `shallow` file because their history is
    /// now complete. Populated by a deepen / `--unshallow` fetch.
    pub new_unshallow: Vec<ObjectId>,
}

/// A single ref update requested by a push.
#[derive(Clone, Debug)]
pub struct PushRefSpec {
    /// The source object to push (`None` for a deletion).
    pub src: Option<ObjectId>,
    /// The destination ref on the remote (e.g. `refs/heads/main`).
    pub dst: String,
    /// Whether a non-fast-forward update is allowed.
    pub force: bool,
    /// Whether this update deletes the remote ref.
    pub delete: bool,
    /// Compare-and-swap expectation: the remote ref's current value must match
    /// this (force-with-lease). `None` disables the value check.
    pub expected_old: Option<ObjectId>,
    /// Force-with-lease expectation that the remote ref does **not** currently
    /// exist. When `true`, a push whose destination already exists on the remote
    /// is rejected as stale (used for "create only" pushes whose lease is the
    /// ref's absence). Independent of [`Self::expected_old`].
    pub expect_absent: bool,
}

/// Options controlling a push.
#[derive(Clone, Default)]
pub struct PushOptions {
    /// Apply all updates atomically (all-or-nothing).
    pub atomic: bool,
    /// Compute results without writing to the remote.
    pub dry_run: bool,
    /// When set, update `refs/remotes/<name>/...` in the local repo after a
    /// successful push (matching Git's post-push tracking ref maintenance).
    pub tracking_remote: Option<String>,
    /// Server-side push options to transmit (`git push --push-option <value>`).
    ///
    /// When non-empty, the negotiated capability list includes `push-options`
    /// and one `push-option <value>` pkt-line per entry is written after the
    /// ref-update command block and before the flush/pack. The remote exposes
    /// these to its hooks via `GIT_PUSH_OPTION_COUNT` / `GIT_PUSH_OPTION_<n>`.
    ///
    /// If this is non-empty but the remote `git-receive-pack` does not advertise
    /// the `push-options` capability, the push fails with
    /// [`crate::error::Error::PushOptionsUnsupported`] (matching Git).
    pub push_options: Vec<String>,
    /// Diagnostic sink for [`crate::diagnostics::Trace::Network`] events.
    pub diagnostics: Option<crate::diagnostics::DiagnosticsHandle>,
    /// When true with [`Self::diagnostics`], push paths may emit network trace events.
    pub network_trace: bool,
}

/// The structured result of a push. Reuses [`PushRefResult`] for per-ref status.
#[derive(Clone, Debug, Default)]
pub struct PushOutcome {
    /// Per-ref resolved results (status, old/new oid, reason).
    pub results: Vec<PushRefResult>,
}

pub use crate::pack_objects::{
    build_pack, build_pack_from_send_list, reachable_closure, PackBuildOptions, PackObjects,
    PackObjectsOptions, PackStats,
};


pub(crate) use crate::pack_objects::build_pack_for_local_fetch;

/// Expand a thin pack by appending missing ref-delta bases from `odb`, matching
/// `git index-pack --fix-thin`.
/// Expand a thin on-disk pack, returning the path to the pack bytes to index.
///
/// When the pack is not thin, returns `pack_path` unchanged. Otherwise writes a
/// fixed pack alongside the input and removes the thin temp file.
pub(crate) fn fix_thin_pack_path(pack_path: &Path, odb: &Odb) -> Result<std::path::PathBuf> {
    use crate::pack_map::PackData;
    use std::ops::Deref;

    let mapped = PackData::open(pack_path)?;
    if !crate::unpack_objects::pack_is_thin(mapped.deref(), odb.hash_algo()) {
        return Ok(pack_path.to_path_buf());
    }
    let data = std::fs::read(pack_path).map_err(Error::Io)?;
    let fixed = fix_thin_pack(data, odb)?;
    let out = pack_path.with_extension("fixed");
    std::fs::write(&out, &fixed).map_err(Error::Io)?;
    let _ = std::fs::remove_file(pack_path);
    Ok(out)
}

pub(crate) fn fix_thin_pack(mut pack: Vec<u8>, odb: &Odb) -> Result<Vec<u8>> {
    let algo = odb.hash_algo();
    let hb = algo.len();
    if !crate::unpack_objects::pack_is_thin(&pack, algo) {
        return Ok(pack);
    }
    if pack.len() < 12 + hb {
        return Err(Error::CorruptObject(
            "thin pack fix: pack too small".to_owned(),
        ));
    }
    let mut missing: Vec<ObjectId> = Vec::new();
    let mut seen = HashSet::new();
    let in_pack = in_pack_whole_object_ids(&pack, algo);
    let mut pos = 12usize;
    let pack_end = pack.len() - hb;
    while pos < pack_end {
        let (type_code, size, header_len) = read_pack_type_size_at(&pack, pos)?;
        let payload_start = pos + header_len;
        match type_code {
            7 => {
                if payload_start + hb > pack_end {
                    return Err(Error::CorruptObject(
                        "thin pack fix: truncated ref-delta".to_owned(),
                    ));
                }
                let base_oid = ObjectId::from_bytes(&pack[payload_start..payload_start + hb])?;
                if !in_pack.contains(&base_oid) && seen.insert(base_oid) {
                    missing.push(base_oid);
                }
                pos = payload_start + hb + zlib_skip(&pack[payload_start + hb..], size)?;
            }
            6 => {
                let (base_len, _) = read_ofs_delta_prefix_len(&pack[payload_start..])?;
                pos =
                    payload_start + base_len + zlib_skip(&pack[payload_start + base_len..], size)?;
            }
            1..=4 => {
                pos = payload_start + zlib_skip(&pack[payload_start..], size)?;
            }
            _ => {
                return Err(Error::CorruptObject(format!(
                    "thin pack fix: unknown type {type_code}"
                )));
            }
        }
    }
    if missing.is_empty() {
        return Ok(pack);
    }
    pack.truncate(pack.len() - hb);
    crate::pack_objects::append_whole_objects_from_odb(&mut pack, odb, &missing)?;
    let old_count = u32::from_be_bytes(
        pack[8..12]
            .try_into()
            .map_err(|_| Error::CorruptObject("thin pack fix: bad object count".to_owned()))?,
    );
    let added = u32::try_from(missing.len())
        .map_err(|_| Error::CorruptObject("thin pack fix: object count overflow".to_owned()))?;
    let new_count = old_count
        .checked_add(added)
        .ok_or_else(|| Error::CorruptObject("thin pack fix: object count overflow".to_owned()))?;
    pack[8..12].copy_from_slice(&new_count.to_be_bytes());
    crate::pack_objects::append_pack_trailer(&mut pack, algo);
    Ok(pack)
}

fn in_pack_whole_object_ids(pack: &[u8], algo: HashAlgo) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let hb = algo.len();
    if pack.len() < 12 + hb {
        return out;
    }
    let pack_end = pack.len() - hb;
    let mut pos = 12usize;
    while pos < pack_end {
        let Ok((type_code, size, header_len)) = read_pack_type_size_at(pack, pos) else {
            break;
        };
        let payload_start = pos + header_len;
        if (1..=4).contains(&type_code) {
            if let Ok(data) = zlib_decompress_fixed(&pack[payload_start..], size) {
                if let Ok(kind) = pack_type_code_to_kind(type_code) {
                    let oid = hash::hash_object(algo, kind, &data);
                    out.insert(oid);
                }
            }
        }
        let advance = match type_code {
            7 if payload_start + hb <= pack_end => {
                header_len + hb + zlib_skip(&pack[payload_start + hb..], size).unwrap_or(0)
            }
            6 => {
                let base_len = read_ofs_delta_prefix_len(&pack[payload_start..])
                    .map(|(n, _)| n)
                    .unwrap_or(0);
                header_len
                    + base_len
                    + zlib_skip(&pack[payload_start + base_len..], size).unwrap_or(0)
            }
            1..=4 => header_len + zlib_skip(&pack[payload_start..], size).unwrap_or(0),
            _ => break,
        };
        pos += advance;
    }
    out
}

fn pack_type_code_to_kind(code: u8) -> Result<ObjectKind> {
    match code {
        1 => Ok(ObjectKind::Commit),
        2 => Ok(ObjectKind::Tree),
        3 => Ok(ObjectKind::Blob),
        4 => Ok(ObjectKind::Tag),
        _ => Err(Error::CorruptObject(format!("bad pack type {code}"))),
    }
}

fn read_pack_type_size_at(pack: &[u8], start: usize) -> Result<(u8, usize, usize)> {
    let first = *pack
        .get(start)
        .ok_or_else(|| Error::CorruptObject("truncated pack header".to_owned()))?;
    let type_code = (first >> 4) & 0x7;
    let mut size = (first & 0x0f) as usize;
    let mut pos = start + 1;
    let mut shift = 4u32;
    let mut cur = first;
    while cur & 0x80 != 0 {
        cur = *pack
            .get(pos)
            .ok_or_else(|| Error::CorruptObject("truncated pack header".to_owned()))?;
        pos += 1;
        size |= ((cur & 0x7f) as usize) << shift;
        shift += 7;
    }
    Ok((type_code, size, pos - start))
}

fn read_ofs_delta_prefix_len(bytes: &[u8]) -> Result<(usize, u64)> {
    let mut pos = 0usize;
    let mut c = *bytes
        .get(pos)
        .ok_or_else(|| Error::CorruptObject("truncated ofs-delta".to_owned()))?;
    pos += 1;
    let mut value = (c & 0x7f) as u64;
    while c & 0x80 != 0 {
        c = *bytes
            .get(pos)
            .ok_or_else(|| Error::CorruptObject("truncated ofs-delta".to_owned()))?;
        pos += 1;
        value = (value + 1) << 7 | (c & 0x7f) as u64;
    }
    Ok((pos, value))
}

fn zlib_skip(bytes: &[u8], expected_size: usize) -> Result<usize> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut dec = ZlibDecoder::new(bytes);
    let mut tmp = Vec::with_capacity(expected_size);
    dec.read_to_end(&mut tmp)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    Ok(dec.total_in() as usize)
}

fn zlib_decompress_fixed(bytes: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut dec = ZlibDecoder::new(bytes);
    let mut out = Vec::with_capacity(expected_size);
    dec.read_to_end(&mut out)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    if out.len() != expected_size {
        return Err(Error::CorruptObject(format!(
            "zlib size mismatch: got {} expected {expected_size}",
            out.len()
        )));
    }
    Ok(out)
}

// and live in a later phase.
pub fn fetch_local(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    opts: &FetchOptions,
) -> Result<FetchOutcome> {
    // Validate that the remote is actually a Git repository: a missing or
    // non-repo path must error (e.g. cloning a bad source) rather than silently
    // fetching nothing. A repo has an `objects` directory (bare or `.git`).
    if !remote_git_dir.join("objects").is_dir() {
        return Err(Error::Message(format!(
            "could not find repository at '{}'",
            remote_git_dir.display()
        )));
    }

    let local_odb = open_odb(local_git_dir);
    let remote_odb = open_odb(remote_git_dir);

    // 1. Enumerate remote refs (with HEAD symref for the default branch).
    let remote_entries = crate::remote::list_refs_from_git_dir(
        remote_git_dir,
        &remote_odb,
        &crate::remote::ListRefsOptions {
            symrefs: true,
            ..Default::default()
        },
    )
    .map_err(crate::error::Error::from)?;

    let mut default_branch = None;
    // remote ref name -> oid (excluding HEAD and peeled `^{}` entries).
    let mut remote_refs: Vec<(String, ObjectId)> = Vec::new();
    for entry in &remote_entries {
        if entry.name == "HEAD" {
            default_branch = entry
                .symref_target
                .as_ref()
                .map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_owned());
            continue;
        }
        if entry.name.ends_with("^{}") {
            continue;
        }
        if !crate::refs::is_valid_fetch_advertised_ref(&entry.name) {
            continue;
        }
        remote_refs.push((entry.name.clone(), entry.oid));
    }

    // 2. Parse refspecs.
    let mut positive: Vec<RefspecItem> = Vec::new();
    let mut negatives: Vec<RefspecItem> = Vec::new();
    for spec in &opts.refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid refspec '{spec}': {e}")))?;
        if item.negative {
            negatives.push(item);
        } else {
            positive.push(item);
        }
    }
    for spec in &opts.negative_refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid negative refspec '{spec}': {e}")))?;
        negatives.push(item);
    }

    // Compute the matched (remote_ref, local_ref, wanted_oid, force) set.
    // `local_ref == None` means "fetch but do not store" (empty dst).
    let mut matched: Vec<MatchedRef> = Vec::new();
    let mut matched_oids: HashSet<ObjectId> = HashSet::new();
    let mut seen_remote_ref: HashSet<String> = HashSet::new();

    for (name, oid) in &remote_refs {
        if name.starts_with("refs/tags/") {
            // Tags are governed by TagMode below, not the head refspecs, unless
            // a refspec explicitly names them. Still allow an explicit refspec
            // match here; TagMode adds the rest.
        }
        if ref_excluded(name, &negatives) {
            continue;
        }
        if let Some(local_ref) = match_positive(name, &positive) {
            if seen_remote_ref.insert(name.clone()) {
                matched_oids.insert(*oid);
                matched.push(MatchedRef {
                    remote_ref: name.clone(),
                    local_ref,
                    oid: *oid,
                    force: refspecs_force(name, &positive),
                    is_tag: name.starts_with("refs/tags/"),
                    advertised_peel: None,
                });
            }
        }
    }

    // TagMode: add tags. We need the closure of objects already being fetched to
    // decide "Following".
    let local_shallow = crate::shallow::load_shallow_boundaries(local_git_dir);
    let remote_shallow = crate::shallow::load_shallow_boundaries(remote_git_dir);

    let mut tag_shallow = local_shallow.clone();
    tag_shallow.extend(remote_shallow.iter().copied());

    apply_tag_mode(
        opts.tags,
        &remote_refs,
        &remote_odb,
        &tag_shallow,
        &negatives,
        &mut matched,
        &mut matched_oids,
        &mut seen_remote_ref,
    )?;

    // 3. Determine wants (matched oids not present locally) and haves (current
    //    local tracking-ref tips) and copy the minimal object set.
    let wants: Vec<ObjectId> = matched_oids
        .iter()
        .copied()
        .filter(|oid| !local_odb.exists(oid))
        .collect();

    let mut haves: Vec<ObjectId> = Vec::new();
    let mut have_seen: HashSet<ObjectId> = HashSet::new();
    for m in &matched {
        if let Some(local_ref) = &m.local_ref {
            if let Ok(old) = crate::refs::resolve_ref(local_git_dir, local_ref) {
                if have_seen.insert(old) {
                    haves.push(old);
                }
            }
        }
    }

    let mut pack_oids: HashSet<ObjectId> = HashSet::new();
    if !wants.is_empty() && !opts.dry_run {
        let remote_cfg = config_for_git_dir(remote_git_dir);
        let pack_opts = PackBuildOptions::for_local_copy(remote_cfg.as_ref());
        let pack = build_pack_for_local_fetch(
            &remote_odb,
            &wants,
            &local_odb,
            &haves,
            &remote_shallow,
            &local_shallow,
            &pack_opts,
        )?;
        pack_oids = crate::index_pack::ingest_received_pack(
            pack,
            &local_odb,
            &crate::index_pack::IngestPackOptions {
                fix_thin: true,
                ..Default::default()
            },
        )?
        .object_ids;
    }

    if opts.initial_remote_fetch && !remote_shallow.is_empty() && !opts.dry_run {
        let remote_shallow_vec: Vec<ObjectId> = remote_shallow.iter().copied().collect();
        crate::shallow::apply_shallow_updates(local_git_dir, &remote_shallow_vec, &[])?;
    }

    if opts.tags == TagMode::Following {
        crate::fetch::retain_following_tags(&local_odb, &mut matched, &pack_oids, &local_shallow)?;
    }

    // 4. Classify and apply ref updates. Ancestry checks use the local repo,
    //    which now contains the fetched objects.
    let local_repo = if opts.dry_run {
        None
    } else {
        crate::repo::Repository::open(local_git_dir, None).ok()
    };

    let mut updates: Vec<RefUpdate> = Vec::new();

    // Prune BEFORE writing the new tips. A stale tracking ref stored as a file
    // (e.g. `refs/remotes/origin/a`) otherwise blocks creating a nested ref the
    // same fetch introduces (`refs/remotes/origin/a/b`) with a "File exists"
    // directory/file conflict (matches `git fetch --prune` ordering).
    if opts.prune {
        prune_tracking_refs(
            local_git_dir,
            &positive,
            &remote_refs,
            opts.dry_run,
            &mut updates,
        )?;
    }

    for m in &matched {
        let Some(local_ref) = &m.local_ref else {
            // dst empty: fetched but not stored. Report as a no-store update.
            updates.push(RefUpdate {
                remote_ref: m.remote_ref.clone(),
                local_ref: None,
                old_oid: None,
                new_oid: Some(m.oid),
                mode: UpdateMode::NoChangeNeeded,
                note: Some("not stored (empty destination)".to_owned()),
            });
            continue;
        };

        let old = crate::refs::resolve_ref(local_git_dir, local_ref).ok();
        let mode = classify_update(old.as_ref(), &m.oid, m.force, m.is_tag, local_repo.as_ref());

        let write = matches!(
            mode,
            UpdateMode::New | UpdateMode::FastForward | UpdateMode::Forced
        );
        if write && !opts.dry_run {
            if !crate::refs::is_valid_storable_ref_name(local_ref) {
                updates.push(RefUpdate {
                    remote_ref: m.remote_ref.clone(),
                    local_ref: Some(local_ref.clone()),
                    old_oid: old,
                    new_oid: Some(m.oid),
                    mode,
                    note: Some("skipped (invalid ref name)".to_owned()),
                });
                continue;
            }
            if m.is_tag && !ref_target_exists(&local_odb, &remote_odb, m.oid) {
                updates.push(RefUpdate {
                    remote_ref: m.remote_ref.clone(),
                    local_ref: Some(local_ref.clone()),
                    old_oid: old,
                    new_oid: Some(m.oid),
                    mode: UpdateMode::NoChangeNeeded,
                    note: Some("skipped (tag target not present locally)".to_owned()),
                });
                continue;
            }
            crate::refs::write_ref(local_git_dir, local_ref, &m.oid)?;
        }

        updates.push(RefUpdate {
            remote_ref: m.remote_ref.clone(),
            local_ref: Some(local_ref.clone()),
            old_oid: old,
            new_oid: Some(m.oid),
            mode,
            note: None,
        });
    }

    crate::fetch::finish_initial_remote_fetch_layout(
        local_git_dir,
        opts,
        default_branch.as_deref(),
    )?;
    let new_shallow = if opts.initial_remote_fetch {
        remote_shallow.iter().copied().collect()
    } else {
        Vec::new()
    };
    Ok(FetchOutcome {
        updates,
        default_branch,
        new_shallow,
        new_unshallow: Vec::new(),
    })
}

/// Push refs and objects from one on-disk git repository into another, entirely
/// in-process (no subprocess, no wire protocol).
///
/// This is the local / `file://` push (send-pack) counterpart to
/// [`fetch_local`]. For each [`PushRefSpec`] it:
///
/// 1. Resolves the source oid from the LOCAL repo (for a non-delete update) and
///    reads the remote's current value of `dst`.
/// 2. Enforces the update rules and produces a [`PushRefResult`] with the right
///    [`crate::push_report::PushRefStatus`]:
///    * `expected_old` set and mismatching the remote's current value →
///      [`PushRefStatus::RejectStale`] (compare-and-swap / force-with-lease).
///    * deletion → succeed when present, or [`PushRefStatus::UpToDate`] when the
///      ref is already gone.
///    * non-fast-forward (remote current is not an ancestor of the source)
///      without `force` → [`PushRefStatus::RejectNonFastForward`]; with `force`
///      it is accepted and reported as forced.
///    * unchanged (remote already at the source) → [`PushRefStatus::UpToDate`].
///    * otherwise [`PushRefStatus::Ok`].
/// 3. For accepted non-delete updates, copies the minimal object closure from the
///    LOCAL odb into the REMOTE odb via [`build_pack`] and
///    [`crate::index_pack::ingest_received_pack`], excluding objects already
///    reachable from the remote's existing ref tips.
/// 4. Applies the ref change on the remote (unless `opts.dry_run`).
///
/// When `opts.atomic` is set and any ref is rejected, no ref or object is
/// written and every otherwise-accepted ref is reported as
/// [`PushRefStatus::AtomicPushFailed`].
///
/// Both repositories must use the same object hash algorithm; the hash width is
/// threaded through [`Odb::hash_algo`] so SHA-256 repos work.
///
/// # Errors
///
/// Returns an error if either repository cannot be opened, if a source object is
/// missing from the local odb, or on I/O failure while writing objects or refs.
//
// TODO(phase: remote transports): `git://`, `http(s)`, and `ssh` push
// (receive-pack handshake + report-status parsing + credential helpers) are out
// of scope here and live in a later phase.
pub fn push_local(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    refs: &[PushRefSpec],
    opts: &PushOptions,
) -> Result<PushOutcome> {
    if !remote_git_dir.join("objects").is_dir() {
        return Err(Error::Message(format!(
            "could not find repository at '{}'",
            remote_git_dir.display()
        )));
    }

    let local_odb = open_odb(local_git_dir);
    let remote_odb = open_odb(remote_git_dir);

    // Ancestry (fast-forward) checks run against the LOCAL repo, where the source
    // commits live. A remote-current oid that is not reachable from the source is
    // simply "not an ancestor", which is the correct non-fast-forward verdict.
    let local_repo = crate::repo::Repository::open(local_git_dir, None).ok();

    // The remote's existing ref tips become the `haves` for pack building, so the
    // copied object closure excludes everything the remote already has.
    let remote_have_tips: Vec<ObjectId> = crate::refs::list_refs(remote_git_dir, "refs/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();

    // First pass: decide each ref's status without mutating anything.
    let mut decisions: Vec<PushDecision> = Vec::with_capacity(refs.len());
    for spec in refs {
        decisions.push(decide_push(
            spec,
            &local_odb,
            remote_git_dir,
            local_repo.as_ref(),
        )?);
    }

    // Atomic: if any update would be rejected, apply none and demote the
    // otherwise-accepted updates to AtomicPushFailed.
    let any_rejected = decisions.iter().any(|d| d.result.status.is_error());
    if opts.atomic && any_rejected {
        for d in &mut decisions {
            if matches!(d.result.status, PushRefStatus::Ok) {
                d.result.status = PushRefStatus::AtomicPushFailed;
                d.apply = false;
            }
        }
        return Ok(PushOutcome {
            results: decisions.into_iter().map(|d| d.result).collect(),
        });
    }

    // Second pass: apply accepted updates (copy objects, then move/delete refs).
    for d in &mut decisions {
        if !d.apply || opts.dry_run {
            continue;
        }
        match &d.action {
            PushAction::Update(src) => {
                let local_cfg = config_for_git_dir(local_git_dir);
                let pack_opts = PackBuildOptions::for_local_push(local_cfg.as_ref());
                let pack = build_pack(&local_odb, &[*src], &remote_have_tips, &pack_opts)?;
                crate::index_pack::ingest_received_pack(
                    pack,
                    &remote_odb,
                    &crate::index_pack::IngestPackOptions {
                        fix_thin: true,
                        ..Default::default()
                    },
                )?;
                crate::refs::write_ref(remote_git_dir, &d.result.remote_ref, src)?;
            }
            PushAction::Delete => {
                crate::refs::delete_ref(remote_git_dir, &d.result.remote_ref)?;
            }
            PushAction::None => {}
        }
    }

    let results: Vec<_> = decisions.into_iter().map(|d| d.result).collect();
    if !opts.dry_run {
        if let Some(remote) = opts.tracking_remote.as_deref() {
            crate::branch_tracking::apply_push_remote_tracking_updates(
                local_git_dir,
                remote,
                &results,
            )?;
        }
    }

    Ok(PushOutcome { results })
}

/// What a single accepted push update does once applied.
enum PushAction {
    /// Copy the closure of `src` to the remote and move the ref to `src`.
    Update(ObjectId),
    /// Delete the remote ref.
    Delete,
    /// No mutation (up-to-date or rejected).
    None,
}

/// A decided-but-not-yet-applied push update.
struct PushDecision {
    result: PushRefResult,
    action: PushAction,
    /// Whether the second pass should apply `action`.
    apply: bool,
}

/// Decide the status of a single [`PushRefSpec`] without mutating either repo.
fn decide_push(
    spec: &PushRefSpec,
    local_odb: &Odb,
    remote_git_dir: &Path,
    local_repo: Option<&crate::repo::Repository>,
) -> Result<PushDecision> {
    let remote_current = crate::refs::resolve_ref(remote_git_dir, &spec.dst).ok();

    // Up-to-date trumps every lease: pushing a non-delete to where the remote ref
    // already points is a no-op that succeeds even when the force-with-lease
    // expectation (a specific `expected_old` value, or `expect_absent`) does not
    // hold — "creating/moving a bookmark to the same place it already is is OK".
    // Must precede both the absence-lease and compare-and-swap checks below.
    if !spec.delete {
        if let Some(src) = spec.src {
            if remote_current == Some(src) {
                return Ok(PushDecision {
                    result: PushRefResult {
                        local_ref: None,
                        remote_ref: spec.dst.clone(),
                        old_oid: remote_current,
                        new_oid: Some(src),
                        forced: false,
                        deletion: false,
                        status: PushRefStatus::UpToDate,
                        message: None,
                    },
                    action: PushAction::None,
                    apply: false,
                });
            }
        }
    }

    // Absence lease (force-with-lease that the ref not exist): once the value is
    // actually changing (handled above), a destination that already exists fails
    // the lease and is rejected as stale.
    if spec.expect_absent && remote_current.is_some() {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: remote_current,
                new_oid: spec.src,
                forced: false,
                deletion: spec.delete,
                status: PushRefStatus::RejectStale,
                message: Some("stale info".to_owned()),
            },
            action: PushAction::None,
            apply: false,
        });
    }

    // Compare-and-swap (force-with-lease): the remote's current value must match
    // the caller's expectation, otherwise reject as stale. A `None` expectation
    // disables the value check.
    if let Some(expected) = spec.expected_old {
        if remote_current != Some(expected) {
            return Ok(PushDecision {
                result: PushRefResult {
                    local_ref: None,
                    remote_ref: spec.dst.clone(),
                    old_oid: remote_current,
                    new_oid: spec.src,
                    forced: false,
                    deletion: spec.delete,
                    status: PushRefStatus::RejectStale,
                    message: Some("stale info".to_owned()),
                },
                action: PushAction::None,
                apply: false,
            });
        }
    }

    if spec.delete {
        let (status, action, apply) = match remote_current {
            Some(_) => (PushRefStatus::Ok, PushAction::Delete, true),
            None => (PushRefStatus::UpToDate, PushAction::None, false),
        };
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: remote_current,
                new_oid: None,
                forced: false,
                deletion: true,
                status,
                message: None,
            },
            action,
            apply,
        });
    }

    // Non-delete updates require a source object that exists locally.
    let Some(src) = spec.src else {
        return Err(Error::Message(format!(
            "push to '{}' has no source object and is not a deletion",
            spec.dst
        )));
    };
    if !local_odb.exists(&src) {
        return Err(Error::Message(format!(
            "source object {src} for '{}' is missing from the local object store",
            spec.dst
        )));
    }

    // Unchanged: the remote is already at the source.
    if remote_current == Some(src) {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: remote_current,
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::UpToDate,
                message: None,
            },
            action: PushAction::None,
            apply: false,
        });
    }

    // New ref: nothing on the remote yet — always allowed.
    let Some(old) = remote_current else {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: None,
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            action: PushAction::Update(src),
            apply: true,
        });
    };

    // Existing ref: fast-forward when the remote's current commit is an ancestor
    // of the source. Otherwise it is a non-fast-forward update, allowed only with
    // force (reported as forced).
    let is_ff = local_repo
        .map(|r| crate::merge_base::is_ancestor(r, old, src).unwrap_or(false))
        .unwrap_or(false);

    if is_ff {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            action: PushAction::Update(src),
            apply: true,
        })
    } else if spec.force {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: true,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            action: PushAction::Update(src),
            apply: true,
        })
    } else {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::RejectNonFastForward,
                message: Some("non-fast-forward".to_owned()),
            },
            action: PushAction::None,
            apply: false,
        })
    }
}

/// A remote ref selected for fetch, with its computed local destination.
pub(crate) struct MatchedRef {
    pub(crate) remote_ref: String,
    /// Destination local tracking ref, or `None` for an empty (no-store) dst.
    pub(crate) local_ref: Option<String>,
    pub(crate) oid: ObjectId,
    pub(crate) force: bool,
    pub(crate) is_tag: bool,
    /// Peeled target from ls-refs `peeled:` (annotated tags), when known.
    pub(crate) advertised_peel: Option<ObjectId>,
}

/// Open an [`Odb`] for a git directory, attaching the git dir so `hash_algo`
/// (and MIDX config) resolve correctly.
pub(crate) fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

fn config_for_git_dir(git_dir: &Path) -> Option<crate::config::ConfigSet> {
    crate::config::ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .ok()
}

/// Match a ref name against the positive refspecs, returning the destination
/// local ref name (`Some(name)`), `None`+stored=false collapsed: returns
/// `Some(Some(dst))` to store, `Some(None)` to fetch-without-store, or `None`
/// when no positive refspec matches.
pub(crate) fn match_positive(refname: &str, positive: &[RefspecItem]) -> Option<Option<String>> {
    for item in positive {
        let Some(src) = item.src.as_deref() else {
            continue;
        };
        if let Some(dst) = apply_refspec(src, item.dst.as_deref(), refname) {
            // Empty dst means "fetch but do not store".
            if dst.is_empty() {
                return Some(None);
            }
            return Some(Some(dst));
        }
    }
    None
}

/// Whether any positive refspec matching `refname` requested force (`+`).
pub(crate) fn refspecs_force(refname: &str, positive: &[RefspecItem]) -> bool {
    positive.iter().any(|item| {
        item.force
            && item
                .src
                .as_deref()
                .is_some_and(|src| apply_refspec(src, item.dst.as_deref(), refname).is_some())
    })
}

/// Whether `refname` is excluded by any negative refspec.
pub(crate) fn ref_excluded(refname: &str, negatives: &[RefspecItem]) -> bool {
    negatives.iter().any(|item| {
        item.src
            .as_deref()
            .is_some_and(|src| glob_matches(src, refname))
    })
}

/// Apply a `<src>[:<dst>]` refspec to `refname`, returning the destination ref.
///
/// Supports a single `*` wildcard (Git's fetch refspec form). When `dst` is
/// `None` the destination equals the matched source (rare for tracking fetches);
/// when `dst` is `Some("")` the empty string is returned (fetch-without-store).
fn apply_refspec(src: &str, dst: Option<&str>, refname: &str) -> Option<String> {
    match src.find('*') {
        Some(star) => {
            let prefix = &src[..star];
            let suffix = &src[star + 1..];
            if !refname.starts_with(prefix)
                || !refname.ends_with(suffix)
                || refname.len() < prefix.len() + suffix.len()
            {
                return None;
            }
            let middle = &refname[prefix.len()..refname.len() - suffix.len()];
            match dst {
                None => Some(refname.to_owned()),
                Some("") => Some(String::new()),
                Some(d) => Some(d.replacen('*', middle, 1)),
            }
        }
        None => {
            if src != refname {
                return None;
            }
            match dst {
                None => Some(refname.to_owned()),
                Some("") => Some(String::new()),
                Some(d) => Some(d.to_owned()),
            }
        }
    }
}

/// Whether `pattern` (a refspec src side, possibly with one `*`) matches `refname`.
fn glob_matches(pattern: &str, refname: &str) -> bool {
    match pattern.find('*') {
        Some(star) => {
            let prefix = &pattern[..star];
            let suffix = &pattern[star + 1..];
            refname.starts_with(prefix)
                && refname.ends_with(suffix)
                && refname.len() >= prefix.len() + suffix.len()
        }
        None => pattern == refname,
    }
}

/// Add tags to the matched set according to [`TagMode`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_tag_mode(
    mode: TagMode,
    remote_refs: &[(String, ObjectId)],
    remote_odb: &Odb,
    shallow_boundaries: &HashSet<ObjectId>,
    negatives: &[RefspecItem],
    matched: &mut Vec<MatchedRef>,
    matched_oids: &mut HashSet<ObjectId>,
    seen_remote_ref: &mut HashSet<String>,
) -> Result<()> {
    if mode == TagMode::None {
        return Ok(());
    }

    // For Following, decide reachability on the source ODB using both the local
    // and remote shallow grafts (unioned by the caller). Post-pack pruning via
    // [`crate::fetch::retain_following_tags`] still drops tags whose objects did
    // not arrive. Using the local ODB here would miss every tip on first fetch.
    let following_closure: HashSet<ObjectId> = if mode == TagMode::Following {
        let roots: Vec<ObjectId> = matched
            .iter()
            .filter(|m| !m.is_tag)
            .map(|m| m.oid)
            .collect();
        crate::fetch::reachable_commits(remote_odb, &roots, shallow_boundaries)
    } else {
        HashSet::new()
    };

    for (name, oid) in remote_refs {
        if !name.starts_with("refs/tags/") {
            continue;
        }
        if seen_remote_ref.contains(name) || ref_excluded(name, negatives) {
            continue;
        }
        let keep = match mode {
            TagMode::All => true,
            TagMode::Following => {
                // Keep when the tag (or what it peels to) is in the fetched
                // closure. Peel annotated tags to their target.
                let peeled = peel_tag_target(remote_odb, *oid)?;
                following_closure.contains(oid) || following_closure.contains(&peeled)
            }
            TagMode::None => false,
        };
        if keep {
            seen_remote_ref.insert(name.clone());
            matched_oids.insert(*oid);
            matched.push(MatchedRef {
                remote_ref: name.clone(),
                local_ref: Some(name.clone()),
                oid: *oid,
                force: false,
                is_tag: true,
                advertised_peel: None,
            });
        }
    }
    Ok(())
}

/// Whether a tag ref's peeled target (or the oid itself for lightweight tags)
/// exists in the local or remote object database.
fn ref_target_exists(local_odb: &Odb, remote_odb: &Odb, tag_oid: ObjectId) -> bool {
    let peel_odb = if local_odb.exists(&tag_oid) {
        local_odb
    } else {
        remote_odb
    };
    let Ok(peeled) = peel_tag_target(peel_odb, tag_oid) else {
        return local_odb.exists(&tag_oid);
    };
    local_odb.exists(&peeled)
}

/// Peel an (annotated) tag to the non-tag object it ultimately points at.
/// Returns the input oid unchanged for non-tag objects or on read failure.
fn peel_tag_target(odb: &Odb, oid: ObjectId) -> Result<ObjectId> {
    let mut current = oid;
    for _ in 0..16 {
        let obj = match odb.read(&current) {
            Ok(o) => o,
            Err(_) => return Ok(current),
        };
        if obj.kind != ObjectKind::Tag {
            return Ok(current);
        }
        current = parse_tag(&obj.data)?.object;
    }
    Ok(current)
}

/// Classify a single ref update into an [`UpdateMode`].
pub(crate) fn classify_update(
    old: Option<&ObjectId>,
    new: &ObjectId,
    force: bool,
    is_tag: bool,
    repo: Option<&crate::repo::Repository>,
) -> UpdateMode {
    let Some(old) = old else {
        return UpdateMode::New;
    };
    if old == new {
        return UpdateMode::UpToDate;
    }
    // Fast-forward when old is an ancestor of new (commit history only).
    let ff = repo
        .map(|r| crate::merge_base::is_ancestor(r, *old, *new).unwrap_or(false))
        .unwrap_or(false);
    if ff && !is_tag {
        return UpdateMode::FastForward;
    }
    if force {
        return UpdateMode::Forced;
    }
    if is_tag {
        return UpdateMode::TagUpdateRejected;
    }
    UpdateMode::NonFastForwardRejected
}

/// Delete local tracking refs whose remote counterpart no longer exists.
///
/// A local tracking ref is a prune candidate when it lives under the destination
/// namespace of some positive wildcard refspec and no current remote ref maps to
/// it. Matches `git fetch --prune` for the common `refs/remotes/<remote>/*` case.
pub(crate) fn prune_tracking_refs(
    local_git_dir: &Path,
    positive: &[RefspecItem],
    remote_refs: &[(String, ObjectId)],
    dry_run: bool,
    updates: &mut Vec<RefUpdate>,
) -> Result<()> {
    // Set of local tracking refs that the current remote justifies.
    let mut live: HashSet<String> = HashSet::new();
    for (name, _) in remote_refs {
        if let Some(Some(dst)) = match_positive(name, positive) {
            live.insert(dst);
        }
    }

    let mut pruned: HashMap<String, ObjectId> = HashMap::new();
    for item in positive {
        let Some(dst) = item.dst.as_deref() else {
            continue;
        };
        if let Some(star) = dst.find('*') {
            // Wildcard refspec: enumerate existing local refs under its
            // destination prefix and prune those the current remote no longer
            // justifies.
            let prefix = &dst[..star];
            for (name, oid) in crate::refs::list_refs(local_git_dir, prefix)? {
                if !name.starts_with(prefix) {
                    continue;
                }
                if !live.contains(&name) {
                    pruned.entry(name).or_insert(oid);
                }
            }
        } else if !live.contains(dst) {
            // Exact refspec (e.g. `refs/heads/a2:refs/remotes/origin/a2`): when
            // the source ref is gone from the remote, `dst` is absent from `live`,
            // so prune the tracking ref if it still exists locally. This is the
            // explicit `git fetch <remote> <branch>` / `--prune` deletion case.
            if let Ok(oid) = crate::refs::resolve_ref(local_git_dir, dst) {
                pruned.entry(dst.to_owned()).or_insert(oid);
            }
        }
    }

    for (name, oid) in pruned {
        if !dry_run {
            crate::refs::delete_ref(local_git_dir, &name)?;
        }
        updates.push(RefUpdate {
            remote_ref: String::new(),
            local_ref: Some(name),
            old_oid: Some(oid),
            new_oid: None,
            mode: UpdateMode::DeletedMissing,
            note: Some("pruned (gone on remote)".to_owned()),
        });
    }
    Ok(())
}
