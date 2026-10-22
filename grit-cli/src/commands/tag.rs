//! `grit tag` — list tags, or create / delete one.

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ref_name_messages::tag_short_name_error_message;
use anyhow::{bail, Context, Result};
use grit_lib::commit::signing::should_sign_tag;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::porcelain::tag::{
    create_tag, delete_tag, list_tags, CreateTagRequest, TagAnnotation, TagKind, TagSummary,
};
use grit_lib::refs::check_ref_format::validate_tag_short_name;
use grit_lib::refs::listing::{RefQuery, RefSort, SortKey};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;
use time::OffsetDateTime;

/// Sort order for `grit tag` list mode.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum, PartialEq, Eq)]
pub enum TagListSort {
    /// `version:refname` (default).
    #[default]
    Version,
    /// Creator date (tagger or committer).
    Date,
    /// Ref name.
    Name,
}

impl TagListSort {
    fn sort_key(self) -> SortKey {
        match self {
            Self::Version => SortKey::Version,
            Self::Date => SortKey::Date,
            Self::Name => SortKey::Name,
        }
    }
}

/// Result of `grit tag`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TagOutcome {
    /// Tags matching the list query.
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

/// Identity fields in JSON tag annotations.
#[derive(Serialize, JsonSchema)]
pub struct TagIdentEntry {
    pub name: String,
    pub email: String,
}

/// Annotated tag metadata in list JSON.
#[derive(Serialize, JsonSchema)]
pub struct TagAnnotationEntry {
    pub subject: String,
    pub message: String,
    pub tagger: TagIdentEntry,
    /// Tagger Unix timestamp.
    pub date: i64,
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

impl HumanRender for TagOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            TagOutcome::List { tags } => {
                if tags.is_empty() {
                    outln!(w, "No tags yet.");
                    return Ok(());
                }
                for tag in tags {
                    if let Some(subject) = tag
                        .annotation
                        .as_ref()
                        .map(|a| a.subject.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        outln!(w, "  {}  {}", tag.name, subject);
                    } else {
                        outln!(w, "  {}", tag.name);
                    }
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
                signed,
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
    list_patterns: Vec<String>,
    sort: TagListSort,
    reverse: bool,
    contains: Option<String>,
    points_at: Vec<String>,
    limit: Option<usize>,
    now: OffsetDateTime,
) -> Result<TagOutcome> {
    let repo = context::discover()?;
    match name {
        Some(name) if delete => remove(&repo, &name),
        Some(name) => create(&repo, &name, message, object, sign, signing_key, now),
        None => list(
            &repo,
            list_patterns,
            sort,
            reverse,
            contains,
            points_at,
            limit,
        ),
    }
}

fn list(
    repo: &Repository,
    patterns: Vec<String>,
    sort: TagListSort,
    reverse: bool,
    contains: Option<String>,
    points_at: Vec<String>,
    limit: Option<usize>,
) -> Result<TagOutcome> {
    let mut contains_ids = Vec::new();
    if let Some(rev) = contains {
        let oid = if rev == "HEAD" {
            match resolve_head(repo.git_dir()).context("could not resolve HEAD for --contains")? {
                HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
                HeadState::Branch { .. } => bail!("HEAD has no commit for --contains"),
                HeadState::Invalid => bail!("HEAD is in an unknown state"),
            }
        } else {
            resolve_revision(repo, &rev).context("could not resolve --contains")?
        };
        contains_ids.push(oid);
    }
    list_with_query(
        repo,
        RefQuery {
            patterns,
            sort: RefSort {
                key: sort.sort_key(),
                reverse,
            },
            limit,
            contains: contains_ids,
            points_at: resolve_points_at(repo, &points_at)?,
            ..RefQuery::default()
        },
    )
}

fn resolve_points_at(
    repo: &Repository,
    revs: &[String],
) -> Result<Vec<grit_lib::objects::ObjectId>> {
    revs.iter()
        .map(|rev| {
            resolve_revision(repo, rev)
                .with_context(|| format!("could not resolve --points-at {rev}"))
        })
        .collect()
}

fn list_with_query(repo: &Repository, query: RefQuery) -> Result<TagOutcome> {
    let tags = list_tags(repo, &query)
        .context("could not list tags")?
        .into_iter()
        .map(tag_entry_from_summary)
        .collect();
    Ok(TagOutcome::List { tags })
}

fn tag_entry_from_summary(tag: TagSummary) -> TagEntry {
    TagEntry {
        name: tag.name,
        oid: tag.oid.to_hex(),
        kind: tag.kind.into(),
        peeled: tag.peeled.map(|p| p.to_hex()),
        target_kind: tag.target_kind.as_str().to_owned(),
        annotation: tag.annotation.map(annotation_entry),
    }
}

fn annotation_entry(a: TagAnnotation) -> TagAnnotationEntry {
    TagAnnotationEntry {
        subject: a.subject,
        message: a.message,
        tagger: TagIdentEntry {
            name: a.tagger.name,
            email: a.tagger.email,
        },
        date: a.date,
    }
}

fn create(
    repo: &Repository,
    name: &str,
    message: Option<String>,
    object: Option<String>,
    sign: Option<bool>,
    signing_key: Option<String>,
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

    let outcome = create_tag(
        repo,
        &CreateTagRequest {
            name: name.to_owned(),
            target,
            message,
            tagger,
            sign,
            signing_key,
        },
    )
    .map_err(anyhow::Error::msg)?;

    Ok(TagOutcome::Create {
        name: outcome.name,
        oid: outcome.oid.to_hex(),
        kind: outcome.kind.into(),
        peeled: outcome.peeled.map(|p| p.to_hex()),
        signed: outcome.signed,
    })
}

fn remove(repo: &Repository, name: &str) -> Result<TagOutcome> {
    delete_tag(repo, name)?;
    Ok(TagOutcome::Delete {
        name: name.to_owned(),
    })
}
