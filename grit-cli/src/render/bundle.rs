//! Outcome types and renderers for `grit bundle`.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::ObjectId;
use schemars::JsonSchema;
use serde::Serialize;

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
