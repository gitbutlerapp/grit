//! `grit bundle` — create, verify, list, and unbundle git bundle files.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use grit_lib::bundle::bundle_remote::BundleCreateOptions;
use grit_lib::bundle::{Bundle, BundleError, BundleVerifyReport};
use grit_lib::list_limit::Limited;

use crate::context;
pub use crate::render::bundle::{
    BundleCreateOutcome, BundleListOutcome, BundleRefJson, BundleRefUpdateJson,
    BundleUnbundleOutcome, BundleVerifyOutcome,
};
use crate::ui::filesystem_path_for_user;

/// Create a bundle from the current repository.
pub fn run_create(
    path: PathBuf,
    revs: Vec<String>,
    all: bool,
    dry_run: bool,
) -> Result<BundleCreateOutcome> {
    let repo = context::discover()?;
    if revs.is_empty() && !all {
        bail!("at least one revision is required (use `--all` to bundle every ref)");
    }
    let options = BundleCreateOptions {
        all_refs: all,
        rev_tokens: revs,
    };
    if dry_run {
        let plan = repo
            .bundles()
            .plan_create_from_revs(&options)
            .map_err(map_bundle_error)?;
        let references: Vec<BundleRefJson> = plan
            .refs
            .iter()
            .map(|r| BundleRefJson {
                name: r.name.clone(),
                oid: r.oid.to_hex(),
            })
            .collect();
        let prerequisites = plan.prerequisites.iter().map(|p| p.oid.to_hex()).collect();
        return Ok(BundleCreateOutcome {
            path: filesystem_path_for_user(&path),
            refs: plan.refs.len(),
            dry_run: true,
            object_count: Some(plan.object_count),
            references,
            prerequisites,
        });
    }
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let mut progress = grit_lib::progress::NullProgress;
    let refs = repo
        .bundles()
        .write_from_revs(
            &options,
            &path,
            &mut progress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_bundle_error)?;
    Ok(BundleCreateOutcome {
        path: filesystem_path_for_user(&path),
        refs,
        dry_run: false,
        object_count: None,
        references: Vec::new(),
        prerequisites: Vec::new(),
    })
}

/// Verify a bundle file (optional repository for prerequisite checks).
pub fn run_verify(path: PathBuf) -> Result<BundleVerifyOutcome> {
    let repo = context::discover().ok();
    let bundle = Bundle::open(&path).map_err(map_bundle_error)?;
    let report = match repo.as_ref() {
        Some(r) => bundle.verify(r).map_err(map_bundle_error)?,
        None => BundleVerifyReport::default(),
    };
    Ok(report_to_outcome(&path, &bundle, report))
}

/// Ingest a bundle pack into the current repository (refs are not updated).
pub fn run_unbundle(path: PathBuf, dry_run: bool) -> Result<BundleUnbundleOutcome> {
    let repo = context::discover()?;
    if dry_run {
        let plan = repo
            .bundles()
            .plan_unbundle(&path)
            .map_err(map_bundle_error)?;
        let bundle = Bundle::open(&path).map_err(map_bundle_error)?;
        let references = bundle
            .header()
            .refs
            .iter()
            .map(|(name, oid)| BundleRefJson::from((name.as_str(), *oid)))
            .collect();
        let ref_updates = plan
            .ref_updates
            .iter()
            .map(|u| BundleRefUpdateJson {
                name: u.name.clone(),
                old_oid: u.old_oid.map(|o| o.to_hex()),
                new_oid: u.new_oid.to_hex(),
            })
            .collect();
        let missing_prerequisites = plan
            .verify
            .missing_prerequisites
            .iter()
            .map(|(oid, _)| oid.to_hex())
            .collect();
        return Ok(BundleUnbundleOutcome {
            path: filesystem_path_for_user(&path),
            dry_run: true,
            references,
            objects: None,
            bytes: None,
            object_count: Some(plan.object_count),
            ref_updates,
            ok: Some(plan.verify.is_ok()),
            missing_prerequisites,
        });
    }
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let outcome = repo
        .bundles()
        .unbundle(
            &path,
            &mut grit_lib::progress::NullProgress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_bundle_error)?;
    Ok(BundleUnbundleOutcome {
        path: filesystem_path_for_user(&path),
        dry_run: false,
        references: outcome
            .refs
            .iter()
            .map(|r| BundleRefJson {
                name: r.name.clone(),
                oid: r.oid.to_hex(),
            })
            .collect(),
        objects: Some(outcome.transfer.objects),
        bytes: Some(outcome.transfer.bytes),
        object_count: None,
        ref_updates: Vec::new(),
        ok: None,
        missing_prerequisites: Vec::new(),
    })
}

/// List refs contained in a bundle.
pub fn run_list(path: PathBuf, limit: Option<usize>) -> Result<BundleListOutcome> {
    let bundle = Bundle::open(&path).map_err(map_bundle_error)?;
    let refs: Vec<BundleRefJson> = bundle
        .header()
        .refs
        .iter()
        .map(|(name, oid)| BundleRefJson::from((name.as_str(), *oid)))
        .collect();
    let listed = Limited::from_vec(refs, limit);
    Ok(BundleListOutcome {
        path: filesystem_path_for_user(&path),
        references: listed.items,
        truncated: listed.truncated,
    })
}

fn report_to_outcome(
    path: &Path,
    bundle: &Bundle,
    report: BundleVerifyReport,
) -> BundleVerifyOutcome {
    let header = bundle.header();
    BundleVerifyOutcome {
        path: filesystem_path_for_user(path),
        ok: report.is_ok(),
        hash_algorithm: header.object_format.name().to_owned(),
        references: header
            .refs
            .iter()
            .map(|(name, oid)| BundleRefJson::from((name.as_str(), *oid)))
            .collect(),
        prerequisites: header
            .prerequisites
            .iter()
            .map(|(oid, _)| oid.to_hex())
            .collect(),
        missing_prerequisites: report
            .missing_prerequisites
            .iter()
            .map(|(oid, _)| oid.to_hex())
            .collect(),
    }
}

fn map_bundle_error(err: BundleError) -> anyhow::Error {
    match err {
        BundleError::MissingPrerequisites { commits } => {
            anyhow::Error::new(MissingPrerequisites { commits })
        }
        BundleError::PrerequisitesNotConnected => anyhow::Error::new(MissingPrerequisites {
            commits: Vec::new(),
        }),
        other if other.is_cancelled() => grit_lib::error::Error::Cancelled.into(),
        other => anyhow::Error::msg(other.to_string()),
    }
}

/// Marker error for bundle prerequisite failures (`exit 128`).
#[derive(Debug)]
pub struct MissingPrerequisites {
    /// Missing prerequisite commits when known (empty for disconnected-prerequisite failures).
    pub commits: Vec<(grit_lib::objects::ObjectId, Option<String>)>,
}

impl std::fmt::Display for MissingPrerequisites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.commits.is_empty() {
            return f.write_str("prerequisite commits are not connected to local refs");
        }
        write!(
            f,
            "repository lacks these prerequisite commits: {}",
            self.commits
                .iter()
                .map(|(oid, _)| oid.to_hex())
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

impl std::error::Error for MissingPrerequisites {}
