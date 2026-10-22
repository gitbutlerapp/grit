//! `grit tag` — list tags, or create / delete one.

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ref_name_messages::tag_short_name_error_message;
use anyhow::{bail, Context, Result};
use grit_lib::commit::signing::should_sign_tag;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::porcelain::tag::{
    create_tag, delete_tag, list_tags, CreateTagRequest, TagError, TagKind,
};
use grit_lib::refs::check_ref_format::validate_tag_short_name;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{self, Read, Write};
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
        replaced: Option<String>,
        dry_run: bool,
    },
    /// A tag was deleted.
    Delete { name: String, dry_run: bool },
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
                replaced,
                dry_run,
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
                let verb = if *dry_run { "Would create" } else { "Created" };
                outln!(w, "{verb} tag `{name}`{kind_note} at `{short}`{peel}.");
                if let Some(old) = replaced {
                    let old_short = old.get(..7).unwrap_or(old.as_str());
                    outln!(w, "Replaced previous target `{old_short}`.");
                }
            }
            TagOutcome::Delete { name, dry_run } => {
                let verb = if *dry_run { "Would delete" } else { "Deleted" };
                outln!(w, "{verb} tag `{name}`.");
            }
        }
        Ok(())
    }
}

/// Parsed flags for a single `grit tag` invocation.
pub struct TagRunOptions {
    /// Tag name, or none to list.
    pub name: Option<String>,
    /// Positional or `--object` revision to tag.
    pub target: Option<String>,
    /// Hidden `--object` alias.
    pub object: Option<String>,
    /// Delete instead of create.
    pub delete: bool,
    /// `-m` message.
    pub message: Option<String>,
    /// `-F` message file.
    pub message_file: Option<String>,
    /// `-f` replace existing tag.
    pub force: bool,
    /// Preview without writing refs.
    pub dry_run: bool,
    /// Sign override from `-s` / `--no-sign`.
    pub sign: Option<bool>,
    /// `--signing-key`.
    pub signing_key: Option<String>,
    /// Wall clock for tagger identity.
    pub now: OffsetDateTime,
}

/// Run `grit tag` with the parsed CLI arguments.
pub fn run(opts: TagRunOptions) -> Result<TagOutcome> {
    let repo = context::discover()?;
    match &opts.name {
        None => list(&repo),
        Some(name) if opts.delete => {
            validate_delete_options(&opts)?;
            remove(&repo, name, opts.dry_run)
        }
        Some(name) => create(&repo, name, &opts),
    }
}

fn validate_delete_options(opts: &TagRunOptions) -> Result<()> {
    if opts.target.is_some() {
        bail!("unexpected revision argument with `--delete`");
    }
    if opts.object.is_some() {
        bail!("unexpected `--object` with `--delete`");
    }
    if opts.message.is_some() || opts.message_file.is_some() {
        bail!("annotated tag options cannot be used with `--delete`");
    }
    Ok(())
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

fn create(repo: &Repository, name: &str, opts: &TagRunOptions) -> Result<TagOutcome> {
    if let Err(err) = validate_tag_short_name(name) {
        bail!(tag_short_name_error_message(name, &err));
    }

    let rev = opts.target.clone().or_else(|| opts.object.clone());
    let target_oid = if let Some(rev) = rev {
        resolve_revision(repo, &rev).context("could not resolve tag target")?
    } else {
        match resolve_head(repo.git_dir())? {
            HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
            HeadState::Branch { .. } => bail!("no commits yet to tag"),
            HeadState::Invalid => bail!("HEAD is in an unknown state"),
        }
    };

    let message = read_message(opts.message.clone(), opts.message_file.clone())?;

    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let will_sign =
        should_sign_tag(&config, opts.sign).context("could not read tag signing config")?;
    let needs_annotated = message.is_some() || will_sign;
    let tagger = if needs_annotated {
        Some(
            context::identity(&env, &config, IdentRole::Committer, opts.now)
                .context("could not resolve tagger identity")?,
        )
    } else {
        None
    };

    let outcome = create_tag(
        repo,
        &CreateTagRequest {
            name: name.to_owned(),
            target: target_oid,
            message,
            tagger,
            sign: opts.sign,
            signing_key: opts.signing_key.clone(),
            force: opts.force,
            dry_run: opts.dry_run,
        },
    )
    .map_err(map_tag_error)?;

    Ok(TagOutcome::Create {
        name: outcome.name,
        oid: outcome.oid.to_hex(),
        kind: outcome.kind.into(),
        peeled: outcome.peeled.map(|p| p.to_hex()),
        signed: outcome.signed,
        replaced: outcome.replaced.map(|p| p.to_hex()),
        dry_run: outcome.dry_run,
    })
}

fn remove(repo: &Repository, name: &str, dry_run: bool) -> Result<TagOutcome> {
    delete_tag(repo, name, dry_run).map_err(map_tag_error)?;
    Ok(TagOutcome::Delete {
        name: name.to_owned(),
        dry_run,
    })
}

fn read_message(message: Option<String>, message_file: Option<String>) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(message), _) => Ok(Some(message)),
        (None, None) => Ok(None),
        (None, Some(path)) if path == "-" => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("could not read message from stdin")?;
            Ok(Some(buf))
        }
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map(Some)
            .with_context(|| format!("could not read message file '{path}'")),
    }
}

fn map_tag_error(err: Error) -> anyhow::Error {
    match err {
        Error::Tag(tag_err) => match tag_err {
            TagError::AlreadyExists { name } => {
                anyhow::anyhow!("tag '{name}' already exists (use --force to replace)")
            }
            TagError::InvalidName { detail } => anyhow::anyhow!("invalid tag name: {detail}"),
            TagError::NotFound { name } => anyhow::anyhow!("no tag named '{name}'"),
            TagError::EmptyMessage => anyhow::anyhow!("tag message cannot be empty"),
        },
        other => anyhow::Error::msg(other.to_string()),
    }
}
