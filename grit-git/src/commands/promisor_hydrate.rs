//! Copy missing blobs from the configured promisor remote into the local object store.
//!
//! Used by partial-clone hydration, `sparse-checkout` updates, and `backfill`.

use anyhow::{bail, Context, Result};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::{zero_oid, DiffEntry, DiffStatus};
use grit_lib::objects::{parse_commit, parse_tree, Object, ObjectId, ObjectKind};
use grit_lib::promisor::{
    read_promisor_missing_oids, repo_treats_promisor_packs, write_promisor_marker,
};
use grit_lib::refs;
use grit_lib::repo::Repository;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Once;

use crate::commands::index_pack;
use crate::fetch_transport::{self, with_packet_trace_identity};
use crate::trace_packet;

static LAZY_FETCH_DISABLED_WARN: Once = Once::new();

#[inline]
fn promisor_local_lazy_fetch_prefers_upload_pack() -> bool {
    trace_packet::trace_packet_dest().is_some()
}

/// What a single promisor prefetch before `diff` processing should cover (`t4067`: one `done` line).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PromisorDiffPrefetch {
    pub rename_detection: bool,
    pub break_rewrites: bool,
    pub needs_blob_content: bool,
}

/// Batch-fetch missing blobs for rename detection, `--break-rewrites`, and patch/stat output.
pub(crate) fn prefetch_promisor_for_diff_entries(
    repo: &Repository,
    entries: &[DiffEntry],
    wt_for_content: Option<&Path>,
    opts: PromisorDiffPrefetch,
) {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if !repo_treats_promisor_packs(&repo.git_dir, &cfg) {
        return;
    }
    let z = zero_oid();
    let mut want: HashSet<ObjectId> = HashSet::new();

    if opts.rename_detection {
        let mut add_oids: HashSet<ObjectId> = HashSet::new();
        let mut del_oids: HashSet<ObjectId> = HashSet::new();
        for e in entries {
            match e.status {
                DiffStatus::Added => {
                    if e.new_oid != z {
                        add_oids.insert(e.new_oid);
                    }
                }
                DiffStatus::Deleted => {
                    if e.old_oid != z {
                        del_oids.insert(e.old_oid);
                    }
                }
                _ => {}
            }
        }
        let skip: HashSet<ObjectId> = add_oids.intersection(&del_oids).copied().collect();
        for e in entries {
            match e.status {
                DiffStatus::Added => {
                    if e.new_oid != z && !skip.contains(&e.new_oid) {
                        want.insert(e.new_oid);
                    }
                }
                DiffStatus::Deleted => {
                    if e.old_oid != z && !skip.contains(&e.old_oid) {
                        want.insert(e.old_oid);
                    }
                }
                _ => {}
            }
        }
    }

    if opts.break_rewrites {
        for e in entries {
            if e.status != DiffStatus::Modified {
                continue;
            }
            if e.old_oid != z {
                want.insert(e.old_oid);
            }
            if e.new_oid != z {
                want.insert(e.new_oid);
            }
        }
    }

    if opts.needs_blob_content && wt_for_content.is_none() {
        for e in entries {
            if e.status == DiffStatus::Unmerged {
                continue;
            }
            if e.old_oid != z {
                want.insert(e.old_oid);
            }
            if e.new_oid != z {
                want.insert(e.new_oid);
            }
        }
    }

    if want.is_empty() {
        return;
    }
    let mut v: Vec<ObjectId> = want.into_iter().collect();
    v.sort();
    let _ = try_lazy_fetch_promisor_objects_batch(repo, &v);
}

/// Match Git `git_env_bool("GIT_NO_LAZY_FETCH", 0)`: unset or empty → lazy fetch allowed; `0` /
/// `false` / `no` / `off` → allowed; truthy spellings → disabled. Invalid values error like Git.
pub(crate) fn git_no_lazy_fetch_env_disables_lazy() -> Result<bool> {
    let raw = match std::env::var("GIT_NO_LAZY_FETCH") {
        Err(_) => return Ok(false),
        Ok(s) if s.trim().is_empty() => return Ok(false),
        Ok(s) => s,
    };
    let t = raw.trim();
    let lower = t.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "0" | "false" | "no" | "off" => false,
        "1" | "true" | "yes" | "on" => true,
        _ => bail!("bad boolean environment value '{t}' for 'GIT_NO_LAZY_FETCH'"),
    })
}

pub(crate) fn warn_lazy_fetch_disabled_once() {
    LAZY_FETCH_DISABLED_WARN.call_once(|| {
        eprintln!("warning: lazy fetching disabled; some objects may not be available");
    });
}

/// Whether a promisor client process (e.g. `git diff` after `clone --filter`) may lazy-fetch.
///
/// Matches [`git_no_lazy_fetch_env_disables_lazy`]: unset or empty `GIT_NO_LAZY_FETCH` means
/// fetching is allowed; truthy values disable it. This differs from `upload-pack`'s
/// `pack-objects` child, which Git pins with `GIT_NO_LAZY_FETCH=1` — that environment does not
/// apply to the user's shell (`t4067-diff-partial-clone`, `t0411-clone-from-partial`).
pub(crate) fn promisor_lazy_fetch_allowed_for_client_process() -> Result<bool> {
    Ok(!git_no_lazy_fetch_env_disables_lazy()?)
}

/// Resolved promisor object source: local ODB path or HTTP remote (system `git fetch`).
pub(crate) enum PromisorSource {
    Local(grit_lib::odb::Odb),
    Http { remote: String },
}

/// Resolve `remote.<name>.url` into a [`PromisorSource`] (local ODB or HTTP).
fn open_promisor_remote_named(
    config: &ConfigSet,
    git_dir: &Path,
    name: &str,
) -> Result<Option<PromisorSource>> {
    let base = git_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| git_dir.to_path_buf());
    let url_key = format!("remote.{name}.url");
    let Some(url) = config.get(&url_key) else {
        return Ok(None);
    };
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(Some(PromisorSource::Http {
            remote: name.to_string(),
        }));
    }
    let path = resolve_remote_repo_path(&base, &url)?;
    let objects_dir = if path.join("objects").is_dir() {
        path.join("objects")
    } else if path.file_name().is_some_and(|n| n == ".git") || path.ends_with(".git") {
        path.join("objects")
    } else {
        path.join(".git").join("objects")
    };
    // Keep a promisor entry even when the source path was removed after clone (t0411): lazy fetch
    // uses `upload-pack` against the recorded git dir, not this ODB.
    Ok(Some(PromisorSource::Local(grit_lib::odb::Odb::new(
        &objects_dir,
    ))))
}

/// Ordered list of `(remote_name, source)` for promisor fetches: `extensions.partialclone` remote
/// first (when set), then each `remote.*.promisor=true` remote.
pub(crate) fn list_promisor_remotes(
    config: &ConfigSet,
    git_dir: &Path,
) -> Result<Vec<(String, PromisorSource)>> {
    let mut out: Vec<(String, PromisorSource)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Git's `promisor_remote_init` orders promisor remotes by config appearance but then MOVES the
    // `extensions.partialClone` remote to the TAIL, so other promisor remotes (e.g. an accepted
    // LOP) are tried first and the clone's own remote is the fallback. Mirror that here: collect
    // `remote.*.promisor=true` in config order, deferring the partial-clone remote to the end
    // (`t5710`: an accepted LOP must be lazily fetched from before falling back to origin).
    let partial_clone_remote = config
        .get("extensions.partialclone")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    for e in config.entries() {
        if !e.key.ends_with(".promisor") {
            continue;
        }
        if e.value.as_deref() != Some("true") {
            continue;
        }
        let Some(rest) = e.key.strip_prefix("remote.") else {
            continue;
        };
        let Some((name, _)) = rest.split_once('.') else {
            continue;
        };
        // Defer the partial-clone remote to the tail.
        if partial_clone_remote.as_deref() == Some(name) {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        if let Some(src) = open_promisor_remote_named(config, git_dir, name)? {
            out.push((name.to_string(), src));
        }
    }

    if let Some(name) = partial_clone_remote {
        // The `extensions.partialClone` remote is the lazy-fetch fallback, but an explicit
        // `remote.<name>.promisor = false` opts it out so missing objects are reported as
        // genuinely absent rather than silently re-fetched (`t5601` "partial clone": after
        // `remote.origin.promisor=false`, `cat-file -e <reverted-blob>` must fail).
        let explicitly_disabled = config
            .get(&format!("remote.{name}.promisor"))
            .map(|v| v.trim().eq_ignore_ascii_case("false"))
            .unwrap_or(false);
        if !explicitly_disabled && seen.insert(name.clone()) {
            if let Some(src) = open_promisor_remote_named(config, git_dir, &name)? {
                out.push((name, src));
            }
        }
    }

    Ok(out)
}

/// Locate the first promisor remote (same order as [`list_promisor_remotes`]) and open its object
/// store or HTTP name.
pub(crate) fn find_promisor_source(
    config: &ConfigSet,
    git_dir: &Path,
) -> Result<Option<PromisorSource>> {
    Ok(list_promisor_remotes(config, git_dir)?
        .into_iter()
        .next()
        .map(|(_, s)| s))
}

/// Try to lazy-fetch `oid` from a configured promisor remote into a new promisor pack.
///
/// Returns `Ok(())` when the object is present locally after the attempt. Matches Git's partial
/// clone behavior for `cat-file` / missing object reads.
pub(crate) fn try_lazy_fetch_promisor_object(repo: &Repository, oid: ObjectId) -> Result<()> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if !repo_treats_promisor_packs(&repo.git_dir, &config) {
        bail!("not a promisor repository");
    }
    if git_no_lazy_fetch_env_disables_lazy()? {
        warn_lazy_fetch_disabled_once();
        bail!("lazy fetching disabled");
    }
    if repo.odb.exists_local(&oid) {
        return Ok(());
    }

    for (remote_name, src) in list_promisor_remotes(&config, &repo.git_dir)? {
        match &src {
            PromisorSource::Local(odb) => {
                if !promisor_local_lazy_fetch_prefers_upload_pack() {
                    if let Ok(obj) = odb.read(&oid) {
                        repo.odb.write(obj.kind, &obj.data).with_context(|| {
                            format!("writing lazy-fetched object from {remote_name}")
                        })?;
                        if repo.odb.exists_local(&oid) {
                            write_promisor_pack_for_local_oids(repo, &[oid]).with_context(
                                || format!("writing promisor pack for {}", oid.to_hex()),
                            )?;
                            register_promisor_default_filter(&repo.git_dir, &remote_name);
                            return Ok(());
                        }
                    }
                }
                let remote_git_dir = odb
                    .objects_dir()
                    .parent()
                    .map(Path::to_path_buf)
                    .with_context(|| "promisor local objects_dir has no parent")?;
                let upload_pack = config
                    .get(&format!("remote.{remote_name}.uploadpack"))
                    .filter(|s| !s.is_empty());
                let pack = match with_packet_trace_identity("fetch", || {
                    fetch_transport::fetch_upload_pack_explicit_wants(
                        &repo.git_dir,
                        &remote_git_dir,
                        upload_pack.as_deref(),
                        &[oid],
                        None,
                    )
                }) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let ingest = index_pack::ingest_pack_bytes(repo, &pack, true);
                if let Ok(ref pack_path) = ingest {
                    let promisor_marker = pack_path.with_extension("promisor");
                    let _ = std::fs::File::create(&promisor_marker);
                } else if promisor_local_lazy_fetch_prefers_upload_pack() {
                    if let Ok(obj) = odb.read(&oid) {
                        let _ = repo.odb.write(obj.kind, &obj.data);
                    }
                } else {
                    let _ = ingest
                        .with_context(|| format!("indexing promisor pack from {remote_name}"))?;
                }
                if repo.odb.read(&oid).is_ok() {
                    register_promisor_default_filter(&repo.git_dir, &remote_name);
                    return Ok(());
                }
            }
            PromisorSource::Http { remote } => {
                if run_http_fetch_objects(repo, remote, &[oid], false).is_ok()
                    && repo.odb.read(&oid).is_ok()
                {
                    register_promisor_default_filter(&repo.git_dir, &remote_name);
                    return Ok(());
                }
            }
        }
    }

    bail!("could not fetch {} from promisor remote", oid.to_hex());
}

fn write_promisor_pack_for_local_oids(repo: &Repository, oids: &[ObjectId]) -> Result<()> {
    let pack_dir = repo.odb.objects_dir().join("pack");
    let pack_path =
        crate::commands::pack_objects::write_partial_clone_promisor_pack(repo, &pack_dir, oids)?;
    std::fs::File::create(pack_path.with_extension("promisor"))?;
    Ok(())
}

/// Lazy-fetch several missing objects in as few promisor negotiations as possible.
///
/// Deduplicates `oids`, skips objects already [`Odb::exists_local`], and uses a single
/// `upload-pack` round-trip when the promisor source is local (`t4067-diff-partial-clone`).
pub(crate) fn try_lazy_fetch_promisor_objects_batch(
    repo: &Repository,
    oids: &[ObjectId],
) -> Result<()> {
    let mut need: Vec<ObjectId> = oids
        .iter()
        .copied()
        .filter(|o| !repo.odb.exists_local(o))
        .collect();
    need.sort();
    need.dedup();
    if need.is_empty() {
        return Ok(());
    }

    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if !repo_treats_promisor_packs(&repo.git_dir, &config) {
        bail!("not a promisor repository");
    }
    if git_no_lazy_fetch_env_disables_lazy()? {
        warn_lazy_fetch_disabled_once();
        bail!("lazy fetching disabled");
    }

    for (remote_name, src) in list_promisor_remotes(&config, &repo.git_dir)? {
        need.retain(|o| !repo.odb.exists_local(o));
        if need.is_empty() {
            return Ok(());
        }
        match &src {
            PromisorSource::Local(odb) => {
                if !promisor_local_lazy_fetch_prefers_upload_pack() {
                    let mut copied_oids: Vec<ObjectId> = Vec::new();
                    for oid in &need {
                        if let Ok(obj) = odb.read(oid) {
                            if repo.odb.write(obj.kind, &obj.data).is_ok() {
                                copied_oids.push(*oid);
                            }
                        }
                    }
                    if !copied_oids.is_empty() {
                        // Objects fetched from a promisor remote are promisor objects: record
                        // them in a `.promisor`-marked pack (like the single-object path) so a
                        // later `gc`/`repack --exclude-promisor-objects` keeps them in the
                        // promisor pack instead of dropping them. Writing them only as loose
                        // objects made them indistinguishable from local commits and a partial
                        // clone's gc would silently delete the freshly fetched blob (t5616
                        // "manual prefetch of missing objects"). Do NOT register a default
                        // partial-clone filter here: a plain (unfiltered) promisor remote must
                        // not gain a `blob:none` filter just because we lazily copied an object
                        // (t0410 "fetching from another promisor remote").
                        write_promisor_pack_for_local_oids(repo, &copied_oids)
                            .with_context(|| "writing promisor pack for lazily fetched objects")?;
                        need.retain(|o| !repo.odb.exists(o));
                        if need.is_empty() {
                            return Ok(());
                        }
                    }
                }
                let remote_git_dir = odb
                    .objects_dir()
                    .parent()
                    .map(Path::to_path_buf)
                    .with_context(|| "promisor local objects_dir has no parent")?;
                let upload_pack = config
                    .get(&format!("remote.{remote_name}.uploadpack"))
                    .filter(|s| !s.is_empty());
                let pack = match with_packet_trace_identity("fetch", || {
                    fetch_transport::fetch_upload_pack_explicit_wants(
                        &repo.git_dir,
                        &remote_git_dir,
                        upload_pack.as_deref(),
                        &need,
                        None,
                    )
                }) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let ingest = index_pack::ingest_pack_bytes(repo, &pack, true);
                if let Ok(ref pack_path) = ingest {
                    let promisor_marker = pack_path.with_extension("promisor");
                    let _ = std::fs::File::create(&promisor_marker);
                } else if promisor_local_lazy_fetch_prefers_upload_pack() {
                    // Thin packs can reference OIDs the client does not have yet; indexing then
                    // fails even though `upload-pack` traced negotiation. Copy loose objects
                    // directly from the promisor ODB (same end state as non-traced lazy fetch).
                    for oid in &need {
                        if repo.odb.exists_local(oid) {
                            continue;
                        }
                        if let Ok(obj) = odb.read(oid) {
                            let _ = repo.odb.write(obj.kind, &obj.data);
                        }
                    }
                } else {
                    let _ = ingest
                        .with_context(|| format!("indexing promisor pack from {remote_name}"))?;
                }
                register_promisor_default_filter(&repo.git_dir, &remote_name);
                // After a successful fetch the wanted objects are present, even if they landed in a
                // `.promisor`-marked pack (which `exists_local` deliberately skips). Use `exists`,
                // which includes promisor packs, so the batch is considered satisfied.
                need.retain(|o| !repo.odb.exists(o));
                if need.is_empty() {
                    return Ok(());
                }
            }
            PromisorSource::Http { remote } => {
                if run_http_fetch_objects(repo, remote, &need, false).is_ok() {
                    register_promisor_default_filter(&repo.git_dir, &remote_name);
                    need.retain(|o| !repo.odb.exists(o));
                    if need.is_empty() {
                        return Ok(());
                    }
                }
            }
        }
    }

    if need.is_empty() {
        Ok(())
    } else {
        bail!(
            "could not fetch {} object(s) from promisor remote",
            need.len()
        )
    }
}

/// Mirror Git's `partial_clone_register` for a lazy fetch: the promisor fetch always uses
/// `--filter=blob:none`, and Git records that filter as the default for the remote (only if the
/// remote does not already have a `partialCloneFilter`). This is what makes a server that lazily
/// fetched from its LOP later advertise `partialCloneFilter=blob:none` for that remote (`t5710`).
fn register_promisor_default_filter(git_dir: &Path, remote_name: &str) {
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let key = format!("remote.{remote_name}.partialclonefilter");
    if cfg.get(&key).filter(|v| !v.is_empty()).is_some() {
        return;
    }
    let config_path = git_dir.join("config");
    let mut file = match ConfigFile::from_path(&config_path, ConfigScope::Local) {
        Ok(Some(f)) => f,
        Ok(None) => match ConfigFile::parse(&config_path, "", ConfigScope::Local) {
            Ok(f) => f,
            Err(_) => return,
        },
        Err(_) => return,
    };
    if file
        .set(
            &format!("remote.{remote_name}.partialCloneFilter"),
            "blob:none",
        )
        .is_ok()
    {
        let _ = file.write();
    }
}

fn resolve_remote_repo_path(base: &Path, url: &str) -> Result<PathBuf> {
    let path_str = url.strip_prefix("file://").unwrap_or(url);
    let p = Path::new(path_str);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    // Prefer a canonical path when the directory exists; if the source was removed after clone
    // (t0411), keep the configured path so `upload-pack` can still be invoked the same way Git does.
    Ok(p.canonicalize().unwrap_or(p))
}

/// Drop promisor-marker entries for blobs already present locally so
/// `rev-list --missing=print` matches Git.
pub(crate) fn trim_promisor_marker_to_missing_local(dest: &Repository) -> Result<()> {
    let mut oids: HashSet<ObjectId> = read_promisor_missing_oids(&dest.git_dir)
        .into_iter()
        .collect();
    oids.retain(|oid| !dest.odb.exists(oid));
    write_promisor_marker(&dest.git_dir, &oids).map_err(|e| anyhow::anyhow!(e))
}

/// After `sparse-checkout set` / `add`, materialize tip blobs matching `patterns` from the
/// promisor remote when this is a partial clone (`grit-promisor-missing` is non-empty).
pub(crate) fn hydrate_sparse_patterns_after_sparse_checkout_update(
    repo: &Repository,
    patterns: &[String],
    cone_mode: bool,
) -> Result<()> {
    if read_promisor_missing_oids(&repo.git_dir).is_empty() {
        return Ok(());
    }
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let Some(promisor) = find_promisor_source(&config, &repo.git_dir)? else {
        return Ok(());
    };
    hydrate_sparse_tip_blobs_from_promisor(repo, &promisor, patterns, cone_mode)?;
    trim_promisor_marker_to_missing_local(repo)
}

/// Copy blobs under `HEAD` matching sparse-checkout `patterns` from the promisor remote.
pub(crate) fn hydrate_sparse_tip_blobs_from_promisor(
    dest: &Repository,
    promisor: &PromisorSource,
    patterns: &[String],
    cone_mode: bool,
) -> Result<()> {
    let head_oid = refs::resolve_ref(&dest.git_dir, "HEAD")?;
    let obj = read_or_fetch_promisor_object(dest, promisor, head_oid, "HEAD for sparse hydration")?;
    if obj.kind != ObjectKind::Commit {
        return Ok(());
    }
    let commit = parse_commit(&obj.data)?;
    let mut need = Vec::new();
    let mut seen_trees = HashSet::new();
    let mut seen_blobs = HashSet::new();
    collect_sparse_missing_blobs_from_tree(
        dest,
        promisor,
        commit.tree,
        "",
        patterns,
        cone_mode,
        &mut seen_trees,
        &mut seen_blobs,
        &mut need,
    )?;
    flush_promisor_blob_batches(dest, promisor, &mut need, 50_000)
}

/// Copy blobs under `tree_oid` matching sparse-checkout `patterns` from the promisor remote.
///
/// Like [`hydrate_sparse_tip_blobs_from_promisor`] but hydrates an explicit tree (e.g. a checkout
/// target) rather than the current `HEAD` commit. Used so a `checkout` into a partial clone with
/// sparse-checkout enabled only fetches the blobs needed for the sparse working set instead of
/// every blob in the tree (t5620 `backfill --sparse without cone mode`).
pub(crate) fn hydrate_sparse_tree_blobs_from_promisor(
    dest: &Repository,
    promisor: &PromisorSource,
    tree_oid: ObjectId,
    patterns: &[String],
    cone_mode: bool,
) -> Result<()> {
    let mut need = Vec::new();
    let mut seen_trees = HashSet::new();
    let mut seen_blobs = HashSet::new();
    collect_sparse_missing_blobs_from_tree(
        dest,
        promisor,
        tree_oid,
        "",
        patterns,
        cone_mode,
        &mut seen_trees,
        &mut seen_blobs,
        &mut need,
    )?;
    flush_promisor_blob_batches(dest, promisor, &mut need, 50_000)
}

/// Copy every blob under `tree_oid` that is not yet present in the local ODB (loose or pack).
pub(crate) fn hydrate_tree_blobs_from_promisor(
    dest: &Repository,
    promisor: &PromisorSource,
    tree_oid: ObjectId,
) -> Result<()> {
    let mut need = Vec::new();
    let mut seen_trees = HashSet::new();
    let mut seen_blobs = HashSet::new();
    collect_all_missing_blobs_from_tree(
        dest,
        promisor,
        tree_oid,
        &mut seen_trees,
        &mut seen_blobs,
        &mut need,
    )?;
    flush_promisor_blob_batches(dest, promisor, &mut need, 50_000)
}

/// Lazily fetch every tree and blob reachable from the given commits from the
/// promisor remote, so a subsequent `rev-list --objects --missing=error` walk
/// finds the full reachable set present (t5616 fsck before/after subtree fetch).
///
/// Returns `Ok(false)` (no-op) when the repository is not a promisor clone or no
/// promisor remote is configured.
pub(crate) fn hydrate_reachable_trees_blobs_from_commits(
    dest: &Repository,
    commits: &[ObjectId],
) -> Result<bool> {
    let config = ConfigSet::load(Some(&dest.git_dir), true).unwrap_or_default();
    if !repo_treats_promisor_packs(&dest.git_dir, &config) {
        return Ok(false);
    }
    if git_no_lazy_fetch_env_disables_lazy()? {
        warn_lazy_fetch_disabled_once();
        bail!("lazy fetching disabled");
    }
    let Some(promisor) = find_promisor_source(&config, &dest.git_dir)? else {
        return Ok(false);
    };

    let mut need = Vec::new();
    let mut seen_trees = HashSet::new();
    let mut seen_blobs = HashSet::new();
    for commit_oid in commits {
        let obj = match dest.odb.read(commit_oid) {
            Ok(o) => o,
            Err(_) => {
                read_or_fetch_promisor_object(dest, &promisor, *commit_oid, "commit for hydration")?
            }
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        collect_all_missing_blobs_from_tree(
            dest,
            &promisor,
            commit.tree,
            &mut seen_trees,
            &mut seen_blobs,
            &mut need,
        )?;
    }
    flush_promisor_blob_batches(dest, &promisor, &mut need, 50_000)?;
    Ok(true)
}

/// Copy every blob reachable from `HEAD`'s tree from the promisor remote.
pub(crate) fn hydrate_head_tree_blobs_from_promisor(
    dest: &Repository,
    promisor: &PromisorSource,
) -> Result<()> {
    let head_oid = refs::resolve_ref(&dest.git_dir, "HEAD")?;
    let obj = read_or_fetch_promisor_object(dest, promisor, head_oid, "HEAD for hydration")?;
    if obj.kind != ObjectKind::Commit {
        return Ok(());
    }
    let commit = parse_commit(&obj.data)?;
    let mut need = Vec::new();
    let mut seen_trees = HashSet::new();
    let mut seen_blobs = HashSet::new();
    collect_all_missing_blobs_from_tree(
        dest,
        promisor,
        commit.tree,
        &mut seen_trees,
        &mut seen_blobs,
        &mut need,
    )?;
    flush_promisor_blob_batches(dest, promisor, &mut need, 50_000)
}

#[allow(clippy::too_many_arguments)]
fn collect_sparse_missing_blobs_from_tree(
    dest: &Repository,
    promisor: &PromisorSource,
    tree_oid: ObjectId,
    prefix: &str,
    patterns: &[String],
    cone_mode: bool,
    seen_trees: &mut HashSet<ObjectId>,
    seen_blobs: &mut HashSet<ObjectId>,
    need: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen_trees.insert(tree_oid) {
        return Ok(());
    }
    let tree_obj =
        read_or_fetch_promisor_object(dest, promisor, tree_oid, "tree for sparse hydration")?;
    if tree_obj.kind != ObjectKind::Tree {
        return Ok(());
    }
    for entry in parse_tree(&tree_obj.data)? {
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
        let pat_path = if is_dir {
            format!("{rel}/")
        } else {
            rel.clone()
        };
        let included = if is_dir && !cone_mode {
            true
        } else {
            super::sparse_checkout::path_matches_sparse_patterns(&pat_path, patterns, cone_mode)
        };
        if !included {
            continue;
        }
        if is_dir {
            collect_sparse_missing_blobs_from_tree(
                dest, promisor, entry.oid, &rel, patterns, cone_mode, seen_trees, seen_blobs, need,
            )?;
            continue;
        }
        if dest.odb.exists_local(&entry.oid) {
            continue;
        }
        if !seen_blobs.insert(entry.oid) {
            continue;
        }
        need.push(entry.oid);
    }
    Ok(())
}

fn collect_all_missing_blobs_from_tree(
    dest: &Repository,
    promisor: &PromisorSource,
    tree_oid: ObjectId,
    seen_trees: &mut HashSet<ObjectId>,
    seen_blobs: &mut HashSet<ObjectId>,
    need: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen_trees.insert(tree_oid) {
        return Ok(());
    }
    let tree_obj = read_or_fetch_promisor_object(dest, promisor, tree_oid, "tree for hydration")?;
    if tree_obj.kind != ObjectKind::Tree {
        return Ok(());
    }
    for entry in parse_tree(&tree_obj.data)? {
        if entry.mode == 0o160000 {
            continue;
        }
        if (entry.mode & 0o170000) == 0o040000 {
            collect_all_missing_blobs_from_tree(
                dest, promisor, entry.oid, seen_trees, seen_blobs, need,
            )?;
            continue;
        }
        if dest.odb.exists_local(&entry.oid) {
            continue;
        }
        if !seen_blobs.insert(entry.oid) {
            continue;
        }
        need.push(entry.oid);
    }
    Ok(())
}

fn read_or_fetch_promisor_object(
    repo: &Repository,
    promisor: &PromisorSource,
    oid: ObjectId,
    purpose: &str,
) -> Result<Object> {
    if let Ok(obj) = repo.odb.read(&oid) {
        return Ok(obj);
    }

    match promisor {
        PromisorSource::Local(odb) => {
            let obj = odb
                .read(&oid)
                .with_context(|| format!("promisor remote missing object {}", oid.to_hex()))?;
            repo.odb
                .write(obj.kind, &obj.data)
                .with_context(|| format!("writing promised object {}", oid.to_hex()))?;
        }
        PromisorSource::Http { remote } => {
            run_http_fetch_objects(repo, remote, &[oid], true)
                .with_context(|| format!("fetching promised object {}", oid.to_hex()))?;
        }
    }

    repo.odb
        .read(&oid)
        .with_context(|| format!("reading {purpose}"))
}

fn flush_promisor_blob_batches(
    repo: &Repository,
    promisor: &PromisorSource,
    need: &mut Vec<ObjectId>,
    min_batch: usize,
) -> Result<()> {
    let min_batch = min_batch.max(1);
    let mut batch: Vec<ObjectId> = Vec::new();
    for oid in need.drain(..) {
        batch.push(oid);
        if batch.len() >= min_batch {
            flush_promisor_blob_batch(repo, promisor, &mut batch)?;
        }
    }
    flush_promisor_blob_batch(repo, promisor, &mut batch)?;
    Ok(())
}

/// Copy up to `batch.len()` blobs from the promisor into `repo`, emit trace2 `promisor fetch_count`,
/// then clear `batch`.
pub(crate) fn flush_promisor_blob_batch(
    repo: &Repository,
    promisor: &PromisorSource,
    batch: &mut Vec<ObjectId>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let count = batch.len();
    match promisor {
        PromisorSource::Local(odb) => {
            // When packet tracing is active (e.g. `GIT_TRACE_PACKET` during `checkout HEAD^` on a
            // partial clone, t5601 #108), perform a real `upload-pack` negotiation for the whole
            // batch so it produces a single `fetch> done` line and a `total_rounds`=1 trace2 event,
            // matching upstream Git's single-round lazy fetch. Otherwise just copy loose objects.
            if promisor_local_lazy_fetch_prefers_upload_pack() {
                let oids: Vec<ObjectId> = std::mem::take(batch);
                try_lazy_fetch_promisor_objects_batch(repo, &oids)?;
            } else {
                let mut fetched = Vec::new();
                for oid in batch.drain(..) {
                    let obj = odb.read(&oid).with_context(|| {
                        format!("could not fetch {} from promisor remote", oid.to_hex())
                    })?;
                    repo.odb
                        .write(obj.kind, &obj.data)
                        .with_context(|| format!("writing {}", oid.to_hex()))?;
                    fetched.push(oid);
                }
                if !fetched.is_empty() {
                    write_promisor_pack_for_local_oids(repo, &fetched)?;
                }
            }
        }
        PromisorSource::Http { remote } => {
            let oids: Vec<ObjectId> = std::mem::take(batch);
            run_http_fetch_objects(repo, remote, &oids, false)?;
            for oid in &oids {
                let _ = repo
                    .odb
                    .read(oid)
                    .with_context(|| format!("object {} not present after fetch", oid.to_hex()))?;
            }
        }
    }

    if let Ok(p) = std::env::var("GIT_TRACE2_EVENT") {
        if !p.is_empty() {
            let _ = crate::trace2_write_json_data_line(
                &p,
                "promisor",
                "fetch_count",
                &count.to_string(),
            );
        }
    }

    Ok(())
}

fn run_http_fetch_objects(
    repo: &Repository,
    remote: &str,
    oids: &[ObjectId],
    use_partial_filter: bool,
) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let url = config
        .get(&format!("remote.{remote}.url"))
        .with_context(|| format!("remote.{remote}.url is not configured"))?;
    let filter_spec = if use_partial_filter {
        config.get(&format!("remote.{remote}.partialclonefilter"))
    } else {
        None
    };
    let http_ctx = crate::http_client::HttpClientContext::from_config_set(&config)?;
    let refspecs: Vec<String> = oids.iter().map(ObjectId::to_hex).collect();
    let options = crate::http_smart::HttpFetchOptions {
        filter_spec: filter_spec.clone(),
        refetch: true,
        ..Default::default()
    };
    crate::http_smart::http_fetch_pack(&repo.git_dir, &url, &refspecs, true, &options, &http_ctx)?;
    Ok(())
}
