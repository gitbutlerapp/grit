//! `grit bundle` — create, verify, and list git bundle files.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use grit_lib::bundle::{Bundle, BundleError, BundleVerifyReport};
use grit_lib::bundle_remote;
use grit_lib::objects::ObjectId;
use serde::Serialize;

use crate::context;
use crate::output::{HumanRender, MarkdownRender};

/// Outcome of `grit bundle create`.
#[derive(Serialize)]
pub struct BundleCreateOutcome {
    pub path: String,
    pub refs: usize,
}

/// Outcome of `grit bundle verify`.
#[derive(Serialize)]
pub struct BundleVerifyOutcome {
    pub path: String,
    pub ok: bool,
    pub hash_algorithm: String,
    pub references: Vec<BundleRefJson>,
    pub prerequisites: Vec<String>,
    pub missing_prerequisites: Vec<String>,
}

/// Outcome of `grit bundle list`.
#[derive(Serialize)]
pub struct BundleListOutcome {
    pub path: String,
    pub references: Vec<BundleRefJson>,
}

#[derive(Serialize)]
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
    fn render_human(&self) {
        println!("Wrote bundle {} ({} refs).", self.path, self.refs);
    }
}

impl HumanRender for BundleVerifyOutcome {
    fn render_human(&self) {
        if self.ok {
            println!("{} is valid", self.path);
            println!(
                "The bundle uses this hash algorithm: {}",
                self.hash_algorithm
            );
            if self.references.is_empty() {
                println!("The bundle contains no refs.");
            } else {
                println!("The bundle contains {} refs:", self.references.len());
                for reference in &self.references {
                    println!("{} {}", reference.oid, reference.name);
                }
            }
        } else {
            eprintln!("{} is invalid", self.path);
            for oid in &self.missing_prerequisites {
                eprintln!("missing prerequisite {oid}");
            }
        }
    }
}

impl MarkdownRender for BundleCreateOutcome {
    fn render_markdown(&self) {
        println!("## Bundle create\n");
        println!("- **path:** `{}`", self.path);
        println!("- **refs:** {}", self.refs);
    }
}

impl MarkdownRender for BundleVerifyOutcome {
    fn render_markdown(&self) {
        println!("## Bundle verify\n");
        println!("- **path:** `{}`", self.path);
        println!("- **ok:** {}", self.ok);
        println!("- **hash_algorithm:** {}", self.hash_algorithm);
        if !self.references.is_empty() {
            println!("\n| Ref | OID |");
            println!("| --- | --- |");
            for reference in &self.references {
                println!("| `{}` | `{}` |", reference.name, reference.oid);
            }
        }
    }
}

impl MarkdownRender for BundleListOutcome {
    fn render_markdown(&self) {
        println!("## Bundle list\n");
        println!("- **path:** `{}`\n", self.path);
        if self.references.is_empty() {
            println!("No refs in bundle header.\n");
            return;
        }
        println!("| Ref | OID |");
        println!("| --- | --- |");
        for reference in &self.references {
            println!("| `{}` | `{}` |", reference.name, reference.oid);
        }
        println!();
    }
}

impl HumanRender for BundleListOutcome {
    fn render_human(&self) {
        for reference in &self.references {
            println!("{} {}", reference.oid, reference.name);
        }
    }
}

/// Create a bundle from the current repository.
pub fn run_create(path: PathBuf, revs: Vec<String>) -> Result<BundleCreateOutcome> {
    let repo = context::discover()?;
    if revs.is_empty() {
        bail!("at least one revision is required");
    }
    let refs = bundle_remote::write_bundle_from_rev_specs(&repo, &path, &revs)
        .map_err(map_lib_error)?;
    Ok(BundleCreateOutcome {
        path: path.display().to_string(),
        refs,
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

/// List refs contained in a bundle.
pub fn run_list(path: PathBuf) -> Result<BundleListOutcome> {
    let bundle = Bundle::open(&path).map_err(map_bundle_error)?;
    Ok(BundleListOutcome {
        path: path.display().to_string(),
        references: bundle
            .header()
            .refs
            .iter()
            .map(|(name, oid)| BundleRefJson::from((name.as_str(), *oid)))
            .collect(),
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

/// Exit code for [`MissingPrerequisites`].
pub const MISSING_PREREQUISITE_EXIT: i32 = 128;
