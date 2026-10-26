//! Fetch and remote helpers for on-disk bundle files (clone / `grit fetch` from a `.bundle`).

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::bundle::{plan_bundle_create, Bundle, BundleCreatePlan, BundleError, BundleSpec};
use crate::error::{Error, Result};
use crate::fetch::{NoProgress, Progress};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::transfer::{
    apply_tag_mode, classify_update, prune_tracking_refs, FetchOptions, FetchOutcome, RefUpdate,
    TagMode, TransferStats, UpdateMode,
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

/// Ref tips and revision tokens for [`write_bundle_with_options`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleCreateOptions {
    /// When true, list `HEAD` and every ref under `refs/` in the bundle header (like `git bundle create --all`).
    pub all_refs: bool,
    /// Revision arguments (tips, ranges, exclusions). Ignored when `all_refs` is true.
    pub rev_tokens: Vec<String>,
}

impl BundleCreateOptions {
    /// Build options from positional revision tokens only.
    #[must_use]
    pub fn from_rev_tokens(rev_tokens: Vec<String>) -> Self {
        Self {
            all_refs: false,
            rev_tokens,
        }
    }
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
    write_bundle_with_options(
        repo,
        path,
        &BundleCreateOptions::from_rev_tokens(rev_tokens.to_vec()),
    )
}

/// Resolve [`BundleSpec`] from create options without writing a file.
///
/// When [`BundleCreateOptions::all_refs`] is true (or `--all` appears among `rev_tokens`), the bundle
/// header lists `HEAD` and every ref under `refs/`, matching `git bundle create --all`.
/// Only commits are walked here; objects are enumerated once, by the pack writer.
///
/// # Errors
///
/// Propagates rev-list, ref resolution, and empty-ref failures.
pub fn resolve_bundle_spec(repo: &Repository, options: &BundleCreateOptions) -> Result<BundleSpec> {
    let (rev_tokens, all_from_token) = strip_all_rev_flag(&options.rev_tokens);
    let all_refs = options.all_refs || all_from_token;

    let (positive_specs, negative_specs) = split_revision_specs_for_bundle(&rev_tokens);

    if positive_specs.is_empty() && !all_refs {
        return Err(Error::Message("at least one revision is required".into()));
    }

    let rev_opts = crate::rev_list::RevListOptions {
        boundary: true,
        all_refs,
        ..Default::default()
    };
    let revs = crate::rev_list::rev_list(repo, &positive_specs, &negative_specs, &rev_opts)?;

    let include = if all_refs {
        refs_for_bundle_header(repo)?
    } else {
        include_refs_from_rev_specs(repo, &positive_specs, &negative_specs, &revs)?
    };

    if include.is_empty() {
        return Err(map_bundle_err(BundleError::EmptyRefs));
    }

    Ok(BundleSpec {
        include,
        exclude: revs.boundary_commits.clone(),
        filter: None,
    })
}

/// Plan bundle creation from revision options without writing a file.
///
/// # Errors
///
/// Same as [`resolve_bundle_spec`] and [`plan_bundle_create`].
pub fn plan_create_with_options(
    repo: &Repository,
    options: &BundleCreateOptions,
) -> Result<BundleCreatePlan> {
    let spec = resolve_bundle_spec(repo, options)?;
    plan_bundle_create(repo, &spec).map_err(map_bundle_err)
}

/// Write a bundle file using ref-set flags and revision tokens.
///
/// # Errors
///
/// Propagates rev-list, ref resolution, and bundle write failures.
pub fn write_bundle_with_options(
    repo: &Repository,
    path: &Path,
    options: &BundleCreateOptions,
) -> Result<usize> {
    let spec = resolve_bundle_spec(repo, options)?;
    let file = File::create(path).map_err(Error::Io)?;
    let mut out = BufWriter::new(file);
    crate::bundle::write_bundle(repo, &spec, &mut out).map_err(map_bundle_err)?;
    out.flush().map_err(Error::Io)?;
    Ok(spec.include.len())
}

fn strip_all_rev_flag(tokens: &[String]) -> (Vec<String>, bool) {
    let mut rev_tokens = Vec::new();
    let mut all_refs = false;
    for token in tokens {
        if token == "--all" {
            all_refs = true;
        } else {
            rev_tokens.push(token.clone());
        }
    }
    (rev_tokens, all_refs)
}

fn refs_for_bundle_header(repo: &Repository) -> Result<Vec<(ObjectId, String)>> {
    let mut include = Vec::new();
    if let Ok(head) = crate::refs::resolve_ref(repo.git_dir(), "HEAD") {
        include.push((head, "HEAD".to_owned()));
    }
    for (name, oid) in crate::refs::list_refs(repo.git_dir(), "refs/")? {
        include.push((oid, name));
    }
    Ok(include)
}

fn include_refs_from_rev_specs(
    repo: &Repository,
    positive_specs: &[String],
    negative_specs: &[String],
    revs: &crate::rev_list::RevListResult,
) -> Result<Vec<(ObjectId, String)>> {
    let commits: HashSet<ObjectId> = revs.commits.iter().copied().collect();
    let excluded: HashSet<ObjectId> = negative_specs
        .iter()
        .filter_map(|spec| crate::rev_parse::resolve_revision(repo, spec).ok())
        .collect();
    // A commit tip is bundled when that commit is sent. An annotated tag (or a tree or
    // blob) is bundled unless it is excluded itself, even when its commit is a
    // prerequisite: the recipient still needs the tag object and ref.
    let tip_is_sent = |oid: ObjectId| match repo.odb().peel_tags(oid) {
        Ok((commit, crate::objects::ObjectKind::Commit)) if commit == oid => {
            commits.contains(&commit)
        }
        Ok(_) => !excluded.contains(&oid),
        Err(_) => false,
    };
    let mut include: Vec<(ObjectId, String)> = Vec::new();
    for spec in positive_specs {
        if let Ok(oid) = crate::rev_parse::resolve_revision(repo, spec) {
            if tip_is_sent(oid) {
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
    Ok(include)
}

fn display_ref_for_spec(repo: &Repository, spec: &str, _oid: ObjectId) -> Result<String> {
    if spec.starts_with("refs/") {
        return Ok(spec.to_owned());
    }
    let exists = |name: &str| crate::refs::resolve_ref(repo.git_dir(), name).is_ok();
    if let Some(name) = crate::worktree_ref::sole_dwim_ref_name(exists, spec) {
        return Ok(name);
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
    fetch_from_bundle_with_progress(
        local_git_dir,
        bundle_path,
        opts,
        &mut NoProgress,
        &crate::progress::NeverCancel,
    )
}

/// Like [`fetch_from_bundle`], reporting bundle receive and index-pack progress.
///
/// The local repository is opened with [`FetchOptions::environment`], so objects land in its
/// `GIT_OBJECT_DIRECTORY` when that is set.
pub fn fetch_from_bundle_with_progress(
    local_git_dir: &Path,
    bundle_path: &Path,
    opts: &FetchOptions,
    progress: &mut dyn Progress,
    cancel: &dyn crate::progress::Cancel,
) -> Result<FetchOutcome> {
    let repo = crate::fetch::open_fetch_target(local_git_dir, opts)?;
    fetch_from_bundle_into(&repo, bundle_path, opts, progress, cancel)
}

/// Like [`fetch_from_bundle_with_progress`], unbundling into `repo`'s object store.
pub(crate) fn fetch_from_bundle_into(
    repo: &Repository,
    bundle_path: &Path,
    opts: &FetchOptions,
    progress: &mut dyn Progress,
    cancel: &dyn crate::progress::Cancel,
) -> Result<FetchOutcome> {
    crate::progress::poll_cancel(progress, cancel)?;
    let bundle = Bundle::open(bundle_path).map_err(map_bundle_err)?;
    bundle
        .verify(repo)
        .map_err(map_bundle_err)?
        .into_result()
        .map_err(map_bundle_err)?;

    let (transfer, pack_oids) = if opts.dry_run {
        (TransferStats::default(), HashSet::new())
    } else {
        crate::progress::poll_cancel(progress, cancel)?;
        let unbundled = bundle
            .unbundle_with_progress(repo, progress, cancel)
            .map_err(map_bundle_err)?;
        crate::progress::poll_cancel(progress, cancel)?;
        (unbundled.transfer, unbundled.pack_object_ids)
    };

    let header = bundle.header();
    let remote_refs = header.refs.clone();
    let init_default_branch = repo.config().ok().and_then(|cfg| {
        cfg.get_last_entry("init.defaultBranch")
            .and_then(|e| e.value.clone())
            .filter(|s| !s.is_empty())
    });
    let default_branch =
        default_branch_from_bundle_head(&header.refs, init_default_branch.as_deref());

    let mut effective = opts.clone();
    if effective.remote_name.is_none() && effective.fetch_source.is_none() {
        effective.fetch_source = Some(bundle_path.to_string_lossy().into_owned());
    }

    crate::progress::poll_cancel(progress, cancel)?;
    complete_bundle_fetch(
        repo,
        &remote_refs,
        default_branch,
        &effective,
        &HashSet::new(),
        pack_oids,
        transfer,
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

#[allow(clippy::too_many_arguments)]
fn complete_bundle_fetch(
    local: &Repository,
    remote_refs: &[(String, ObjectId)],
    default_branch: Option<String>,
    opts: &FetchOptions,
    remote_shallow: &HashSet<ObjectId>,
    pack_oids: HashSet<ObjectId>,
    transfer: TransferStats,
) -> Result<FetchOutcome> {
    let local_git_dir = local.git_dir();
    let local_odb = local.odb();
    let remote_head_oid = crate::transfer::remote_head_oid_from_advertised(remote_refs);
    let filtered_remote_refs: Vec<(String, ObjectId)> = remote_refs
        .iter()
        .filter(|(name, _)| {
            name != "HEAD"
                && !name.ends_with("^{}")
                && crate::refs::is_valid_fetch_advertised_ref(name)
        })
        .cloned()
        .collect();

    let crate::transfer::FetchMatchSet {
        mut matched,
        positive,
        negatives,
        explicit_refspec_fetch,
    } = crate::transfer::build_fetch_matched_set(opts, &filtered_remote_refs, remote_head_oid)?;
    let mut matched_oids: HashSet<ObjectId> = matched.iter().map(|m| m.oid).collect();
    let mut seen_remote_ref: HashSet<String> =
        matched.iter().map(|m| m.remote_ref.clone()).collect();

    let local_shallow = crate::shallow::load_shallow_boundaries(local_git_dir);
    let mut tag_shallow = local_shallow.clone();
    tag_shallow.extend(remote_shallow.iter().copied());

    let tags_present = if opts.tags == TagMode::Following {
        crate::fetch::tags_already_local(local.refs(), remote_refs)
    } else {
        HashSet::new()
    };
    let _following_only = apply_tag_mode(
        opts.tags,
        remote_refs,
        local_odb,
        &tag_shallow,
        &negatives,
        &tags_present,
        &mut matched,
        &mut matched_oids,
        &mut seen_remote_ref,
    )?;

    if opts.tags == TagMode::Following {
        crate::fetch::retain_following_tags(local_odb, &mut matched, &pack_oids, &local_shallow)?;
    }

    let local_repo = (!opts.dry_run).then_some(local);

    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut prune_batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
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
                Some(&mut prune_batch)
            },
        )?;
    }
    if !opts.dry_run && !prune_batch.is_empty() {
        crate::refs::commit_ref_store_batch(local_git_dir, &prune_batch)?;
    }

    let mut store_batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
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
        let mode = classify_update(
            old.as_ref(),
            &m.oid,
            m.force,
            m.dest_is_tag,
            local_repo,
            None,
        );

        let write = matches!(
            mode,
            UpdateMode::New | UpdateMode::FastForward | UpdateMode::Forced
        );
        crate::transfer::enforce_fetch_ref_update(
            local_ref,
            old,
            m.oid,
            mode,
            opts,
            explicit_refspec_fetch,
            local_repo,
        )?;
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
        crate::transfer::apply_fetch_ref_updates(
            local_git_dir,
            &store_batch,
            &mut updates,
            crate::transfer::FetchRefRejectHint {
                prune: opts.prune,
                positive: &positive,
                remote_refs,
                remote_name: opts.remote_name.as_deref(),
            },
        )?;
    }

    crate::fetch::finish_initial_remote_fetch_layout(
        local_git_dir,
        opts,
        default_branch.as_deref(),
    )?;
    crate::transfer::finish_anonymous_url_fetch(
        local_git_dir,
        opts,
        explicit_refspec_fetch,
        default_branch.as_deref(),
        remote_refs,
        &matched,
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
        transfer,
    })
}

fn map_bundle_err(e: BundleError) -> Error {
    match e {
        BundleError::Cancelled => Error::Cancelled,
        BundleError::MissingPrerequisite { .. } | BundleError::PrerequisitesNotConnected => {
            Error::BundleMissingPrerequisites
        }
        other => Error::Message(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;

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

    #[test]
    fn write_bundle_from_rev_specs_records_tag_dwim_as_refs_tags() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::process::Command::new("git")
            .current_dir(dir)
            .args(["init", "-q", "-b", "main", "."])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git init");
        std::fs::write(dir.join("a.txt"), b"a\n").unwrap();
        assert!(std::process::Command::new("git")
            .current_dir(dir)
            .args(["add", "a.txt"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                "one",
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .current_dir(dir)
            .args(["tag", "lite"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap()
            .success());

        let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo");
        let bundle_path = dir.join("t1.bundle");
        write_bundle_from_rev_specs(&repo, &bundle_path, &["lite".to_owned(), "main".to_owned()])
            .expect("write bundle");

        let header = crate::bundle::Bundle::open(&bundle_path)
            .expect("open bundle")
            .header()
            .clone();
        let names: Vec<_> = header.refs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"refs/tags/lite"),
            "expected refs/tags/lite in {names:?}"
        );
        assert!(
            !names.contains(&"refs/heads/lite"),
            "must not record tag as branch: {names:?}"
        );
        assert!(names.contains(&"refs/heads/main"));
    }
}
