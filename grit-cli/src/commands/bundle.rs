//! `grit bundle` — create, verify, list, and unbundle git bundle files.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use grit_lib::bundle::bundle_remote::{self, BundleCreateOptions};
use grit_lib::bundle::{Bundle, BundleError, BundleVerifyReport};
use grit_lib::list_limit::Limited;
use grit_lib::objects::ObjectId;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Outcome of `grit bundle create`.
#[derive(Serialize, JsonSchema)]
pub struct BundleCreateOutcome {
    pub path: String,
    pub refs: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(skip_serializing_if = "is_false")]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub object_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<BundleRefJson>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
}

/// Outcome of `grit bundle verify`.
#[derive(Serialize, JsonSchema)]
pub struct BundleVerifyOutcome {
    pub path: String,
    pub ok: bool,
    pub hash_algorithm: String,
    pub references: Vec<BundleRefJson>,
    pub prerequisites: Vec<String>,
    pub missing_prerequisites: Vec<String>,
}

/// Outcome of `grit bundle list`.
#[derive(Serialize, JsonSchema)]
pub struct BundleListOutcome {
    pub path: String,
    pub references: Vec<BundleRefJson>,
    /// True when the bundle header held more refs than returned (`-n` / `--limit`).
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub truncated: bool,
}

/// One ref update that would be applied when unbundling with ref writes enabled.
#[derive(Serialize, JsonSchema)]
pub struct BundleRefUpdateJson {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_oid: Option<String>,
    pub new_oid: String,
}

/// Outcome of `grit bundle unbundle`.
#[derive(Serialize, JsonSchema)]
pub struct BundleUnbundleOutcome {
    pub path: String,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(skip_serializing_if = "is_false")]
    pub dry_run: bool,
    pub references: Vec<BundleRefJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub objects: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub object_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub ref_updates: Vec<BundleRefUpdateJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub missing_prerequisites: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct BundleRefJson {
    pub name: String,
    pub oid: String,
}

impl From<(&str, ObjectId)> for BundleRefJson {
    fn from((name, oid): (&str, ObjectId)) -> Self {
        Self {
            name: name.to_owned(),
            oid: oid.to_hex(),
        }
    }
}

impl HumanRender for BundleCreateOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Dry run: would write bundle {} ({} refs, {} objects).",
                self.path,
                self.refs,
                self.object_count.unwrap_or(0)
            );
        } else {
            outln!(w, "Wrote bundle {} ({} refs).", self.path, self.refs);
        }
        Ok(())
    }
}

impl HumanRender for BundleVerifyOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.ok {
            outln!(w, "{} is valid", self.path);
            outln!(
                w,
                "The bundle uses this hash algorithm: {}",
                self.hash_algorithm
            );
            if self.references.is_empty() {
                outln!(w, "The bundle contains no refs.");
            } else {
                outln!(w, "The bundle contains {} refs:", self.references.len());
                for reference in &self.references {
                    outln!(w, "{} {}", reference.oid, reference.name);
                }
            }
        } else {
            eprintln!("{} is invalid", self.path);
            for oid in &self.missing_prerequisites {
                eprintln!("missing prerequisite {oid}");
            }
        }
        Ok(())
    }
}

impl MarkdownRender for BundleCreateOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Bundle create\n");
        outln!(w, "- **path:** `{}`", self.path);
        outln!(w, "- **refs:** {}", self.refs);
        if self.dry_run {
            outln!(w, "- **dry_run:** true");
            if let Some(n) = self.object_count {
                outln!(w, "- **object_count:** {n}");
            }
        }
        Ok(())
    }
}

impl MarkdownRender for BundleVerifyOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Bundle verify\n");
        outln!(w, "- **path:** `{}`", self.path);
        outln!(w, "- **ok:** {}", self.ok);
        outln!(w, "- **hash_algorithm:** {}", self.hash_algorithm);
        if !self.references.is_empty() {
            outln!(w, "\n| Ref | OID |");
            outln!(w, "| --- | --- |");
            for reference in &self.references {
                outln!(w, "| `{}` | `{}` |", reference.name, reference.oid);
            }
        }
        Ok(())
    }
}

impl MarkdownRender for BundleListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Bundle list\n");
        outln!(w, "- **path:** `{}`\n", self.path);
        if self.references.is_empty() {
            outln!(w, "No refs in bundle header.\n");
            return Ok(());
        }
        outln!(w, "| Ref | OID |");
        outln!(w, "| --- | --- |");
        for reference in &self.references {
            outln!(w, "| `{}` | `{}` |", reference.name, reference.oid);
        }
        outln!(w);
        Ok(())
    }
}

impl HumanRender for BundleListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for reference in &self.references {
            outln!(w, "{} {}", reference.oid, reference.name);
        }
        Ok(())
    }
}

impl HumanRender for BundleUnbundleOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Dry run: verified {} ({} objects in pack); {} refs differ from local tips (unbundle does not update refs).",
                self.path,
                self.object_count.unwrap_or(0),
                self.ref_updates.len()
            );
        } else {
            outln!(
                w,
                "Unbundled {} ({} objects, {} bytes).",
                self.path,
                self.objects.unwrap_or(0),
                self.bytes.unwrap_or(0)
            );
        }
        for reference in &self.references {
            outln!(w, "{} {}", reference.oid, reference.name);
        }
        Ok(())
    }
}

impl MarkdownRender for BundleUnbundleOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Bundle unbundle\n");
        outln!(w, "- **path:** `{}`", self.path);
        if self.dry_run {
            outln!(w, "- **dry_run:** true");
            if let Some(n) = self.object_count {
                outln!(w, "- **object_count:** {n}");
            }
        } else if let Some(objects) = self.objects {
            outln!(w, "- **objects:** {objects}");
            outln!(w, "- **bytes:** {}", self.bytes.unwrap_or(0));
        }
        if !self.references.is_empty() {
            outln!(w, "\n| Ref | OID |");
            outln!(w, "| --- | --- |");
            for reference in &self.references {
                outln!(w, "| `{}` | `{}` |", reference.name, reference.oid);
            }
        }
        outln!(w);
        Ok(())
    }
}

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
            .plan_create_with_options(&options)
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
            path: path.display().to_string(),
            refs: plan.refs.len(),
            dry_run: true,
            object_count: Some(plan.object_count),
            references,
            prerequisites,
        });
    }
    let refs =
        bundle_remote::write_bundle_with_options(&repo, &path, &options).map_err(map_lib_error)?;
    Ok(BundleCreateOutcome {
        path: path.display().to_string(),
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
            path: path.display().to_string(),
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
    let outcome = repo
        .bundles()
        .unbundle_with_progress(
            &path,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .map_err(map_bundle_error)?;
    Ok(BundleUnbundleOutcome {
        path: path.display().to_string(),
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
        path: path.display().to_string(),
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
        path: path.display().to_string(),
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
        BundleError::MissingPrerequisite { .. } | BundleError::PrerequisitesNotConnected => {
            anyhow::Error::new(MissingPrerequisites)
        }
        other => anyhow::Error::msg(other.to_string()),
    }
}

fn map_lib_error(err: grit_lib::error::Error) -> anyhow::Error {
    match err {
        grit_lib::error::Error::BundleMissingPrerequisites => {
            anyhow::Error::new(MissingPrerequisites)
        }
        other => other.into(),
    }
}

/// Marker error for bundle prerequisite failures (`exit 128`).
#[derive(Debug)]
pub struct MissingPrerequisites;

impl std::fmt::Display for MissingPrerequisites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("repository lacks these prerequisite commits")
    }
}

impl std::error::Error for MissingPrerequisites {}
