//! `grit tag` — list tags, or create / delete one.

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ref_name_messages::tag_short_name_error_message;
use anyhow::{bail, Context, Result};
use grit_lib::commit::signing::should_sign_tag;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::porcelain::tag::{create_tag, delete_tag, list_tags, CreateTagRequest, TagKind};
use grit_lib::refs::check_ref_format::validate_tag_short_name;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::tree_sha256::TreeSha256Options;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;
use time::OffsetDateTime;

/// Result of `grit tag`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TagOutcome {
    /// All tags in the repository, sorted by name.
    List { tags: Vec<TagEntry> },
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
    },
    /// A tag was deleted.
    Delete { name: String },
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

impl std::fmt::Display for TagRefKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
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
}

impl HumanRender for TagOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            TagOutcome::List { tags } => {
                if tags.is_empty() {
                    outln!(w, "No tags yet.");
                    return Ok(());
                }
                for tag in tags {
                    outln!(w, "  {}", tag.name);
                }
            }
            TagOutcome::Create {
                name,
                tree_sha256,
                tree_sha256_files,
                tree_sha256_submodules,
                tree_sha256_hash_ms,
                ..
            } => {
                outln!(w, "Created tag {name}");
                if let Some(digest) = tree_sha256 {
                    let files = tree_sha256_files.unwrap_or(0);
                    let subs = tree_sha256_submodules.unwrap_or(0);
                    let ms = tree_sha256_hash_ms.unwrap_or(0.0);
                    if subs > 0 {
                        outln!(
                            w,
                            "{files} files in {subs} submodules hashed in {ms:.2}ms (tree-sha256 {digest})"
                        );
                    } else {
                        outln!(
                            w,
                            "{files} files hashed in {ms:.2}ms (tree-sha256 {digest})"
                        );
                    }
                }
            }
            TagOutcome::Delete { name } => outln!(w, "Deleted tag {name}"),
        }
        Ok(())
    }
}

impl MarkdownRender for TagOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            TagOutcome::List { tags } => markdown::print_tag_list(w, tags)?,
            TagOutcome::Create {
                name,
                oid,
                kind,
                peeled,
                signed,
                tree_sha256,
                tree_sha256_hash_ms,
                ..
            } => {
                let short = oid.get(..7).unwrap_or(oid.as_str());
                let kind_note = match (*kind, *signed) {
                    (TagRefKind::Annotated, true) => " (annotated, signed)",
                    (TagRefKind::Annotated, false) => " (annotated)",
                    _ => "",
                };
                let peel = peeled
                    .as_ref()
                    .map(|p| format!(" → `{}`", p.get(..7).unwrap_or(p.as_str())))
                    .unwrap_or_default();
                outln!(w, "Created tag `{name}`{kind_note} at `{short}`{peel}.");
                if let (Some(digest), Some(ms)) = (tree_sha256.as_ref(), tree_sha256_hash_ms) {
                    outln!(w, "tree-sha256 `{digest}` (hashed in {ms:.2}ms).");
                }
            }
            TagOutcome::Delete { name } => outln!(w, "Deleted tag `{name}`."),
        }
        Ok(())
    }
}

/// Run `grit tag` with the parsed CLI arguments.
#[allow(clippy::too_many_arguments)]
pub fn run(
    name: Option<String>,
    delete: bool,
    message: Option<String>,
    object: Option<String>,
    sign: Option<bool>,
    signing_key: Option<String>,
    tree_sha256: bool,
    exclude_submodule: Vec<String>,
    now: OffsetDateTime,
) -> Result<TagOutcome> {
    let repo = context::discover()?;
    match name {
        None => list(&repo),
        Some(name) if delete => remove(&repo, &name),
        Some(name) => create(
            &repo,
            &name,
            message,
            object,
            sign,
            signing_key,
            tree_sha256,
            exclude_submodule,
            now,
        ),
    }
}

fn list(repo: &Repository) -> Result<TagOutcome> {
    let tags = list_tags(repo)
        .context("could not list tags")?
        .into_iter()
        .map(|tag| TagEntry {
            name: tag.name,
            oid: tag.oid.to_hex(),
            kind: tag.kind.into(),
            peeled: tag.peeled.map(|p| p.to_hex()),
        })
        .collect();
    Ok(TagOutcome::List { tags })
}

#[allow(clippy::too_many_arguments)]
fn create(
    repo: &Repository,
    name: &str,
    message: Option<String>,
    object: Option<String>,
    sign: Option<bool>,
    signing_key: Option<String>,
    tree_sha256: bool,
    exclude_submodule: Vec<String>,
    now: OffsetDateTime,
) -> Result<TagOutcome> {
    if let Err(err) = validate_tag_short_name(name) {
        bail!(tag_short_name_error_message(name, &err));
    }

    let target = if let Some(rev) = object {
        resolve_revision(repo, &rev).context("could not resolve tag target")?
    } else {
        match resolve_head(repo.git_dir())? {
            HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
            HeadState::Branch { .. } => bail!("no commits yet to tag"),
            HeadState::Invalid => bail!("HEAD is in an unknown state"),
        }
    };

    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let will_sign = should_sign_tag(&config, sign).context("could not read tag signing config")?;
    let needs_annotated = message.is_some() || will_sign;
    let tagger = if needs_annotated {
        Some(
            context::identity(&env, &config, IdentRole::Committer, now)
                .context("could not resolve tagger identity")?,
        )
    } else {
        None
    };

    let tree_sha256_opts = if tree_sha256 || !exclude_submodule.is_empty() {
        let mut opts = TreeSha256Options::default();
        opts.exclude_globs = exclude_submodule;
        Some(opts)
    } else {
        None
    };

    let outcome = create_tag(
        repo,
        &CreateTagRequest {
            name: name.to_owned(),
            target,
            message,
            tagger,
            sign,
            signing_key,
            tree_sha256: tree_sha256_opts,
        },
    )
    .map_err(anyhow::Error::msg)?;

    let (
        tree_sha256,
        tree_sha256_excluded,
        tree_sha256_files,
        tree_sha256_submodules,
        tree_sha256_hash_ms,
    ) = outcome
        .tree_sha256
        .as_ref()
        .map_or((None, None, None, None, None), |ts| {
            (
                Some(ts.claim.digest.to_hex()),
                if ts.claim.excluded.is_empty() {
                    None
                } else {
                    Some(ts.claim.excluded.clone())
                },
                Some(ts.report.files),
                Some(ts.report.submodules),
                Some(ts.hash_elapsed.as_secs_f64() * 1000.0),
            )
        });

    Ok(TagOutcome::Create {
        name: outcome.name,
        oid: outcome.oid.to_hex(),
        kind: outcome.kind.into(),
        peeled: outcome.peeled.map(|p| p.to_hex()),
        signed: outcome.signed,
        tree_sha256,
        tree_sha256_excluded,
        tree_sha256_files,
        tree_sha256_submodules,
        tree_sha256_hash_ms,
    })
}

fn remove(repo: &Repository, name: &str) -> Result<TagOutcome> {
    delete_tag(repo, name)?;
    Ok(TagOutcome::Delete {
        name: name.to_owned(),
    })
}
