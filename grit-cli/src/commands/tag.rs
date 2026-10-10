//! `grit tag` — list tags, or create / delete one.
//!
//! Tags created here are **lightweight**: a `refs/tags/<name>` ref that points
//! at the chosen commit (defaults to HEAD). `grit` is opinionated and skips the
//! annotated-tag ceremony; reach for `grit tag -a` when you need that.

use anyhow::{bail, Context, Result};
use grit_lib::check_ref_format::validate_tag_short_name;

use crate::ref_name_messages::tag_short_name_error_message;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use serde::Serialize;

use crate::context;
use crate::output::{HumanRender, MarkdownRender};

/// Result of `grit tag`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TagOutcome {
    /// All tags in the repository, sorted by name.
    List { tags: Vec<TagEntry> },
    /// A new tag was created.
    Create {
        name: String,
        /// Hex object id the tag points at.
        oid: String,
    },
    /// A tag was deleted.
    Delete { name: String },
}

/// One tag in a `list` outcome.
#[derive(Serialize)]
pub struct TagEntry {
    pub name: String,
    /// Hex object id the tag points at.
    pub oid: String,
}

impl HumanRender for TagOutcome {
    fn render_human(&self) {
        match self {
            TagOutcome::List { tags } => {
                if tags.is_empty() {
                    println!("No tags yet.");
                    return;
                }
                for tag in tags {
                    println!("  {}", tag.name);
                }
            }
            TagOutcome::Create { name, .. } => println!("Created tag {name}"),
            TagOutcome::Delete { name } => println!("Deleted tag {name}"),
        }
    }
}

impl MarkdownRender for TagOutcome {}

/// Run `grit tag` with the parsed CLI arguments.
///
/// * `name = None` → list tags (any `delete` flag is ignored).
/// * `name = Some(_)` with `delete = true` → delete the named tag.
/// * `name = Some(_)` otherwise → create the tag at HEAD.
pub fn run(name: Option<String>, delete: bool) -> Result<TagOutcome> {
    let repo = context::discover()?;
    match name {
        None => list(&repo),
        Some(name) if delete => delete_tag(&repo, &name),
        Some(name) => create(&repo, &name),
    }
}

fn list(repo: &Repository) -> Result<TagOutcome> {
    let mut entries =
        refs::list_refs(&repo.git_dir, "refs/tags/").context("could not list tags")?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let tags = entries
        .into_iter()
        .map(|(refname, oid)| TagEntry {
            name: refname
                .strip_prefix("refs/tags/")
                .unwrap_or(&refname)
                .to_owned(),
            oid: oid.to_hex(),
        })
        .collect();

    Ok(TagOutcome::List { tags })
}

fn create(repo: &Repository, name: &str) -> Result<TagOutcome> {
    if let Err(err) = validate_tag_short_name(name) {
        bail!(tag_short_name_error_message(name, &err));
    }
    let tag_ref = format!("refs/tags/{name}");
    if refs::resolve_ref(&repo.git_dir, &tag_ref).is_ok() {
        bail!("tag '{name}' already exists");
    }

    let base = match resolve_head(&repo.git_dir)? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
        HeadState::Branch { .. } => bail!("no commits yet to tag"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    };

    refs::write_ref(&repo.git_dir, &tag_ref, &base).context("could not create tag")?;
    Ok(TagOutcome::Create {
        name: name.to_owned(),
        oid: base.to_hex(),
    })
}

fn delete_tag(repo: &Repository, name: &str) -> Result<TagOutcome> {
    let tag_ref = format!("refs/tags/{name}");
    if refs::resolve_ref(&repo.git_dir, &tag_ref).is_err() {
        bail!("no tag named '{name}'");
    }

    refs::delete_ref(&repo.git_dir, &tag_ref).context("could not delete tag")?;
    Ok(TagOutcome::Delete {
        name: name.to_owned(),
    })
}
