//! `grit tag` outcomes and rendering.

use anyhow::Result;
use grit_lib::objects::short_hex;
use grit_lib::porcelain::tag::TagKind;
use schemars::JsonSchema;
use serde::Serialize;
use std::fmt;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, Person, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit tag`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TagOutcome {
    /// Tags matching the list query.
    List {
        tags: Vec<TagEntry>,
        /// True when more tags matched than returned (`-n` / `--limit`).
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        truncated: bool,
        #[serde(skip)]
        #[schemars(skip)]
        names: Vec<String>,
    },
    /// A new tag was created.
    Create {
        name: String,
        /// Hex object id the tag ref points at.
        oid: String,
        kind: TagRefKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        peeled: Option<String>,
        /// Whether the tag object carries a cryptographic signature.
        signed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        tree_sha256: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        tree_sha256_excluded: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        tree_sha256_files: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        tree_sha256_submodules: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        tree_sha256_hash_ms: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        replaced: Option<String>,
        dry_run: bool,
    },
    /// A tag was deleted.
    Delete { name: String, dry_run: bool },
    /// One or more annotated tags were verified.
    Verify {
        /// True when every listed tag passed verification.
        ok: bool,
        tags: Vec<TagVerifyEntry>,
    },
}

/// Per-tag result for `grit tag --verify`.
#[derive(Serialize, JsonSchema)]
pub struct TagVerifyEntry {
    pub name: String,
    /// Hex object id of the annotated tag object.
    pub oid: String,
    /// Hex object id from the tag's `object` header.
    pub target: String,
    pub ok: bool,
    pub signature: TagSignatureJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tree_sha256: Option<TagTreeSha256Json>,
    /// Submodule paths that could not be hashed for digest verification.
    pub not_verifiable_submodules: Vec<NotVerifiableSubmoduleJson>,
    /// Wall time spent verifying this tag.
    pub elapsed: TagVerifyTimingJson,
}

#[derive(Serialize, JsonSchema)]
pub struct TagSignatureJson {
    /// `good`, `bad`, `missing`, or `not_checked`.
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub trust: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct TagTreeSha256Json {
    pub claimed: String,
    pub computed: String,
    pub match_digest: bool,
    pub files: usize,
    pub submodules: usize,
    pub excluded: Vec<ExcludedSubmoduleJson>,
    pub bogus_excluded: Vec<String>,
    pub cycles: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ExcludedSubmoduleJson {
    pub path: String,
    pub oid: String,
}

#[derive(Serialize, JsonSchema)]
pub struct NotVerifiableSubmoduleJson {
    pub path: String,
    pub reason: String,
}

#[derive(Serialize, JsonSchema)]
pub struct TagVerifyTimingJson {
    /// Elapsed milliseconds (rounded).
    pub ms: u64,
}

/// Tag ref kind in JSON (`lightweight` or `annotated`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TagRefKind {
    Lightweight,
    Annotated,
}

impl TagRefKind {
    fn label(self) -> &'static str {
        match self {
            Self::Lightweight => "lightweight",
            Self::Annotated => "annotated",
        }
    }
}

impl From<TagKind> for TagRefKind {
    fn from(kind: TagKind) -> Self {
        match kind {
            TagKind::Lightweight => Self::Lightweight,
            TagKind::Annotated => Self::Annotated,
        }
    }
}

impl fmt::Display for TagRefKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Annotated tag metadata in list JSON.
#[derive(Serialize, JsonSchema)]
pub struct TagAnnotationEntry {
    pub subject: String,
    pub message: String,
    pub tagger: Person,
}

/// One tag in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct TagEntry {
    pub name: String,
    /// Hex object id the tag ref points at.
    pub oid: String,
    pub kind: TagRefKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub peeled: Option<String>,
    pub target_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub annotation: Option<TagAnnotationEntry>,
}

fn tag_list_markdown(w: &mut dyn Write, tags: &[TagEntry]) -> Result<()> {
    if tags.is_empty() {
        outln!(w, "No tags yet.");
        return Ok(());
    }
    outln!(w, "## Tags");
    for tag in tags {
        let short = short_hex(&tag.oid);
        let peel = tag
            .peeled
            .as_ref()
            .map(|p| format!(" → `{}`", short_hex(p)))
            .unwrap_or_default();
        let subject = tag
            .annotation
            .as_ref()
            .map(|a| format!(" — {}", a.subject))
            .unwrap_or_default();
        outln!(
            w,
            "- `{}` · {} `{short}`{peel}{subject}",
            tag.name,
            tag.kind
        );
    }
    Ok(())
}

fn render_verify_human(w: &mut dyn Write, entry: &TagVerifyEntry) -> Result<()> {
    let status = if entry.ok { "ok" } else { "FAILED" };
    outln!(
        w,
        "tag {name} ({oid}): {status}",
        name = entry.name,
        oid = entry.oid
    );
    outln!(w, "  target: {}", entry.target);
    outln!(
        w,
        "  signature: {} ({})",
        entry.signature.verdict,
        entry.signature.signer.as_deref().unwrap_or("—")
    );
    if let Some(digest) = &entry.tree_sha256 {
        outln!(
            w,
            "  tree-sha256: claimed {} computed {} ({})",
            digest.claimed,
            digest.computed,
            if digest.match_digest {
                "match"
            } else {
                "mismatch"
            }
        );
        outln!(
            w,
            "  hashed: {} files, {} submodules",
            digest.files,
            digest.submodules
        );
    }
    if !entry.not_verifiable_submodules.is_empty() {
        outln!(w, "  not verifiable submodules:");
        for miss in &entry.not_verifiable_submodules {
            outln!(w, "    {} ({})", miss.path, miss.reason);
        }
    }
    outln!(w, "  elapsed: {} ms", entry.elapsed.ms);
    Ok(())
}

fn render_verify_markdown(w: &mut dyn Write, entry: &TagVerifyEntry) -> Result<()> {
    outln!(w, "### `{}`\n", entry.name);
    outln!(w, "- **ok:** {}", entry.ok);
    outln!(w, "- **oid:** `{}`", entry.oid);
    outln!(w, "- **target:** `{}`", entry.target);
    outln!(w, "- **signature:** `{}`", entry.signature.verdict);
    if let Some(signer) = &entry.signature.signer {
        outln!(w, "- **signer:** `{signer}`");
    }
    if let Some(digest) = &entry.tree_sha256 {
        outln!(
            w,
            "- **tree-sha256:** claimed `{}`, computed `{}` ({})",
            digest.claimed,
            digest.computed,
            if digest.match_digest {
                "match"
            } else {
                "mismatch"
            }
        );
        outln!(
            w,
            "- **hashed:** {} files, {} submodules",
            digest.files,
            digest.submodules
        );
    }
    if !entry.not_verifiable_submodules.is_empty() {
        outln!(w, "- **not verifiable submodules:**");
        for miss in &entry.not_verifiable_submodules {
            outln!(w, "  - `{}` ({})", miss.path, miss.reason);
        }
    }
    outln!(w, "- **elapsed:** {} ms\n", entry.elapsed.ms);
    Ok(())
}

impl HumanRender for TagOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            TagOutcome::List { tags, names, .. } => {
                if tags.is_empty() && names.is_empty() {
                    outln!(w, "No tags yet.");
                    return Ok(());
                }
                for name in names {
                    outln!(w, "  {name}");
                }
                for tag in tags {
                    outln!(w, "  {}", tag.name);
                }
            }
            TagOutcome::Create { name, dry_run, .. } => {
                if *dry_run {
                    outln!(w, "Would create tag {name}");
                } else {
                    outln!(w, "Created tag {name}");
                }
            }
            TagOutcome::Delete { name, dry_run } => {
                if *dry_run {
                    outln!(w, "Would delete tag {name}");
                } else {
                    outln!(w, "Deleted tag {name}");
                }
            }
            TagOutcome::Verify { ok, tags } => {
                for entry in tags {
                    render_verify_human(w, entry)?;
                }
                if !*ok {
                    outln!(w, "Verification failed.");
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for TagOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            TagOutcome::List { tags, .. } => tag_list_markdown(w, tags)?,
            TagOutcome::Create {
                name,
                oid,
                kind,
                peeled,
                signed,
                replaced,
                dry_run,
                ..
            } => {
                let short = short_hex(oid);
                let kind_note = match (*kind, *signed) {
                    (TagRefKind::Annotated, true) => " (annotated, signed)",
                    (TagRefKind::Annotated, false) => " (annotated)",
                    _ => "",
                };
                let peel = peeled
                    .as_ref()
                    .map(|p| format!(" → `{}`", short_hex(p)))
                    .unwrap_or_default();
                let verb = if *dry_run { "Would create" } else { "Created" };
                outln!(w, "{verb} tag `{name}`{kind_note} at `{short}`{peel}.");
                if let Some(old) = replaced {
                    let old_short = short_hex(old);
                    outln!(w, "Replaced previous target `{old_short}`.");
                }
            }
            TagOutcome::Delete { name, dry_run } => {
                let verb = if *dry_run { "Would delete" } else { "Deleted" };
                outln!(w, "{verb} tag `{name}`.");
            }
            TagOutcome::Verify { ok, tags } => {
                outln!(w, "## Tag verify\n");
                outln!(w, "- **ok:** {ok}\n");
                for entry in tags {
                    render_verify_markdown(w, entry)?;
                }
            }
        }
        Ok(())
    }
}
