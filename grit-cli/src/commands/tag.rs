//! `grit tag` — list tags, or create / delete one.

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ref_name_messages::tag_short_name_error_message;
use anyhow::{bail, Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::porcelain::tag::{create_tag, tag_ref_metadata, CreateTagRequest, TagKind};
use grit_lib::refs;
use grit_lib::refs::check_ref_format::validate_tag_short_name;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, HeadState};
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
            TagOutcome::Create { name, .. } => outln!(w, "Created tag {name}"),
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
            } => {
                let short = oid.get(..7).unwrap_or(oid.as_str());
                let kind_note = if *kind == TagRefKind::Annotated {
                    " (annotated)"
                } else {
                    ""
                };
                let peel = peeled
                    .as_ref()
                    .map(|p| format!(" → `{}`", p.get(..7).unwrap_or(p.as_str())))
                    .unwrap_or_default();
                outln!(w, "Created tag `{name}`{kind_note} at `{short}`{peel}.");
            }
            TagOutcome::Delete { name } => outln!(w, "Deleted tag `{name}`."),
        }
        Ok(())
    }
}

/// Run `grit tag` with the parsed CLI arguments.
pub fn run(
    name: Option<String>,
    delete: bool,
    message: Option<String>,
    object: Option<String>,
    now: OffsetDateTime,
) -> Result<TagOutcome> {
    let repo = context::discover()?;
    match name {
        None => list(&repo),
        Some(name) if delete => delete_tag(&repo, &name),
        Some(name) => create(&repo, &name, message, object, now),
    }
}

fn list(repo: &Repository) -> Result<TagOutcome> {
    let mut entries =
        refs::list_refs(repo.git_dir(), "refs/tags/").context("could not list tags")?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let tags = entries
        .into_iter()
        .map(|(refname, oid)| {
            let (kind, peeled) = tag_ref_metadata(repo, &oid);
            TagEntry {
                name: refname
                    .strip_prefix("refs/tags/")
                    .unwrap_or(&refname)
                    .to_owned(),
                oid: oid.to_hex(),
                kind: kind.into(),
                peeled: peeled.map(|p| p.to_hex()),
            }
        })
        .collect();

    Ok(TagOutcome::List { tags })
}

fn create(
    repo: &Repository,
    name: &str,
    message: Option<String>,
    object: Option<String>,
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
    let tagger = if message.is_some() {
        Some(
            context::identity(&env, &config, IdentRole::Committer, now)
                .context("could not resolve tagger identity")?,
        )
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
        },
    )
    .map_err(anyhow::Error::msg)?;

    Ok(TagOutcome::Create {
        name: outcome.name,
        oid: outcome.oid.to_hex(),
        kind: outcome.kind.into(),
        peeled: outcome.peeled.map(|p| p.to_hex()),
    })
}

fn delete_tag(repo: &Repository, name: &str) -> Result<TagOutcome> {
    let tag_ref = format!("refs/tags/{name}");
    if refs::resolve_ref(repo.git_dir(), &tag_ref).is_err() {
        bail!("no tag named '{name}'");
    }

    refs::delete_ref(repo.git_dir(), &tag_ref).context("could not delete tag")?;
    Ok(TagOutcome::Delete {
        name: name.to_owned(),
    })
}
