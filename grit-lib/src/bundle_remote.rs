//! Fetch and remote helpers for on-disk bundle files (clone / `grit fetch` from a `.bundle`).

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::bundle::{Bundle, BundleError, BundleSpec};
use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::repo::Repository;
use crate::transfer::{
    apply_tag_mode, classify_update, match_positive, prune_tracking_refs, ref_excluded,
    refspecs_force, FetchOptions, FetchOutcome, MatchedRef, RefUpdate, TagMode, UpdateMode,
};

const V2_SIGNATURE: &[u8] = b"# v2 git bundle\n";
const V3_SIGNATURE: &[u8] = b"# v3 git bundle\n";

/// Whether `path` begins with a v2 or v3 git bundle signature.
#[must_use]
pub fn is_bundle_path(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.starts_with(V2_SIGNATURE) || bytes.starts_with(V3_SIGNATURE)
}

/// Write a bundle file from revision arguments (supports `A..B` ranges like `git bundle create`).
///
/// # Errors
///
/// Propagates rev-list, ref resolution, and bundle write failures.
pub fn write_bundle_from_rev_specs(
    repo: &Repository,
    path: &Path,
    rev_tokens: &[String],
) -> Result<usize> {
    let (positive_specs, negative_specs) = split_revision_specs_for_bundle(rev_tokens);
    if positive_specs.is_empty() {
        return Err(Error::Message("at least one revision is required".into()));
    }
    let rev_opts = crate::rev_list::RevListOptions {
        boundary: true,
        objects: true,
        ..Default::default()
    };
    let revs = crate::rev_list::rev_list(repo, &positive_specs, &negative_specs, &rev_opts)?;

    let mut include: Vec<(ObjectId, String)> = Vec::new();
    for spec in &positive_specs {
        if let Ok(oid) = crate::rev_parse::resolve_revision(repo, spec) {
            if revs.commits.contains(&oid) || revs.objects.iter().any(|(o, _)| *o == oid) {
                let name = display_ref_for_spec(repo, spec, oid)?;
                if !include.iter().any(|(_, n)| n == &name) {
                    include.push((oid, name));
                }
            }
        }
    }
    if include.is_empty() {
        for oid in &revs.commits {
            include.push((*oid, format!("refs/heads/bundle-tip/{}", oid.to_hex())));
        }
    }
    if include.is_empty() {
        return Err(map_bundle_err(BundleError::EmptyRefs));
    }

    let spec = BundleSpec {
        include,
        exclude: revs.boundary_commits.clone(),
        filter: None,
    };
    let file = File::create(path).map_err(Error::Io)?;
    let mut out = BufWriter::new(file);
    crate::bundle::write_bundle(repo, &spec, &mut out).map_err(map_bundle_err)?;
    out.flush().map_err(Error::Io)?;
    Ok(spec.include.len())
}

fn display_ref_for_spec(repo: &Repository, spec: &str, _oid: ObjectId) -> Result<String> {
    if spec.starts_with("refs/") {
        return Ok(spec.to_owned());
    }
    let resolve = |name: &str| crate::refs::resolve_ref(&repo.git_dir, name).ok();
    let (count, _) = crate::worktree_ref::resolve_ref_dwim(resolve, spec);
    if count == 1 {
        for rule in ["{0}", "refs/{0}", "refs/heads/{0}", "refs/remotes/{0}"] {
            let candidate = rule.replace("{0}", spec);
            if crate::refs::resolve_ref(&repo.git_dir, &candidate).is_ok() {
                return Ok(candidate);
            }
        }
    }
    Ok(format!("refs/heads/{}", spec.trim_start_matches("heads/")))
}

/// Split CLI revision tokens into positive and negative specs for bundle creation.
#[must_use]
pub fn split_revision_specs_for_bundle(tokens: &[String]) -> (Vec<String>, Vec<String>) {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for token in tokens {
        let (mut pos, mut neg) = crate::rev_list::split_revision_token(token);
        positive.append(&mut pos);
        negative.append(&mut neg);
    }
    (positive, negative)
}

/// Fetch from a bundle file into an existing repository (like `git fetch` from a bundle).
///
/// # Errors
///
/// Propagates bundle verification, pack ingest, refspec, and ref update failures.
pub fn fetch_from_bundle(
    local_git_dir: &Path,
    bundle_path: &Path,
    opts: &FetchOptions,
) -> Result<FetchOutcome> {
    let bundle = Bundle::open(bundle_path).map_err(map_bundle_err)?;
    let repo = Repository::open(local_git_dir, None)?;
    bundle
        .verify(&repo)
        .map_err(map_bundle_err)?
        .into_result()
        .map_err(map_bundle_err)?;

    if !opts.dry_run {
        bundle.unbundle(&repo).map_err(map_bundle_err)?;
    }

    let header = bundle.header();
    let remote_refs = header.refs.clone();
    let init_default_branch = repo.config().ok().and_then(|cfg| {
        cfg.get_last_entry("init.defaultBranch")
            .and_then(|e| e.value.clone())
            .filter(|s| !s.is_empty())
    });
    let default_branch =
        default_branch_from_bundle_head(&header.refs, init_default_branch.as_deref());

    complete_bundle_fetch(
        local_git_dir,
        &repo.odb,
        &remote_refs,
        default_branch,
        opts,
        &HashSet::new(),
        HashSet::new(),
    )
}

/// Branch short name to check out after cloning from a bundle, from the bundle `HEAD` OID.
///
/// Matches Git's `guess_remote_head` when `HEAD` is not a symref.
fn default_branch_from_bundle_head(
    refs: &[(String, ObjectId)],
    init_default_branch: Option<&str>,
) -> Option<String> {
    let head_oid = refs
        .iter()
        .find(|(name, _)| name == "HEAD")
        .map(|(_, oid)| *oid)?;

    if let Some(name) = init_default_branch {
        if bundle_branch_tip_matches(refs, name, head_oid) {
            return Some(name.to_owned());
        }
    }

    if init_default_branch != Some("master") && bundle_branch_tip_matches(refs, "master", head_oid)
    {
        return Some("master".to_owned());
    }

    for (name, oid) in refs.iter().rev() {
        if name == "HEAD" {
            continue;
        }
        if let Some(short) = name.strip_prefix("refs/heads/") {
            if *oid == head_oid {
                return Some(short.to_owned());
            }
        }
    }
    None
}

fn bundle_branch_tip_matches(
    refs: &[(String, ObjectId)],
    branch: &str,
    head_oid: ObjectId,
) -> bool {
    let full = format!("refs/heads/{branch}");
    refs.iter()
        .any(|(name, oid)| name == &full && *oid == head_oid)
}

fn complete_bundle_fetch(
    local_git_dir: &Path,
    local_odb: &crate::odb::Odb,
    remote_refs: &[(String, ObjectId)],
    default_branch: Option<String>,
    opts: &FetchOptions,
    remote_shallow: &HashSet<ObjectId>,
    pack_oids: HashSet<ObjectId>,
) -> Result<FetchOutcome> {
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

    let mut matched: Vec<MatchedRef> = Vec::new();
    let mut matched_oids: HashSet<ObjectId> = HashSet::new();
    let mut seen_remote_ref: HashSet<String> = HashSet::new();

    for (name, oid) in remote_refs {
        if name == "HEAD" || name.ends_with("^{}") {
            continue;
        }
        if !crate::refs::is_valid_fetch_advertised_ref(name) {
            continue;
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

    let local_shallow = crate::shallow::load_shallow_boundaries(local_git_dir);
    let mut tag_shallow = local_shallow.clone();
    tag_shallow.extend(remote_shallow.iter().copied());

    apply_tag_mode(
        opts.tags,
        remote_refs,
        local_odb,
        &tag_shallow,
        &negatives,
        &mut matched,
        &mut matched_oids,
        &mut seen_remote_ref,
    )?;

    if opts.tags == TagMode::Following {
        crate::fetch::retain_following_tags(local_odb, &mut matched, &pack_oids, &local_shallow)?;
    }

    let local_repo = if opts.dry_run {
        None
    } else {
        Repository::open(local_git_dir, None).ok()
    };

    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut store_batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
    if opts.prune {
        prune_tracking_refs(
            local_git_dir,
            &positive,
            remote_refs,
            opts.dry_run,
            &mut updates,
            if opts.dry_run {
                None
            } else {
                Some(&mut store_batch)
            },
        )?;
    }

    for m in &matched {
        let Some(local_ref) = &m.local_ref else {
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
            store_batch.push(crate::refs::store::RefUpdate {
                name: local_ref.clone(),
                new_value: Some(crate::refs::store::RawRef::Direct(m.oid)),
                expected: crate::refs::store::Expected::Any,
                reflog: None,
                flags: Default::default(),
            });
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

    if !opts.dry_run && !store_batch.is_empty() {
        crate::refs::commit_ref_store_batch(local_git_dir, &store_batch)?;
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

fn map_bundle_err(e: BundleError) -> Error {
    match e {
        BundleError::MissingPrerequisite { .. } | BundleError::PrerequisitesNotConnected => {
            Error::BundleMissingPrerequisites
        }
        other => Error::Message(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_branch_follows_bundle_head_not_first_branch_line() {
        let main = ObjectId::from_hex("f04545002f192e3ffe2e2f6341abb0665215b0c8").unwrap();
        let feature = ObjectId::from_hex("c48fa981db2b56e99fb87fc3c2827ee4b5186d8f").unwrap();
        let refs = vec![
            ("refs/heads/feature".into(), feature),
            ("refs/heads/main".into(), main),
            ("HEAD".into(), main),
        ];
        assert_eq!(
            default_branch_from_bundle_head(&refs, None).as_deref(),
            Some("main")
        );
    }

    #[test]
    fn split_revision_specs_handles_double_dot_range() {
        let tokens = vec!["HEAD~1..HEAD".to_owned()];
        let (pos, neg) = split_revision_specs_for_bundle(&tokens);
        assert_eq!(pos, vec!["HEAD"]);
        assert_eq!(neg, vec!["HEAD~1"]);
    }
}
