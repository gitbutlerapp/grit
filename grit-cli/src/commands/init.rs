//! `grit init` — create a new, empty repository.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use grit_lib::config::git_path::user_display_path;
use grit_lib::prelude::Environment;
use grit_lib::objects::HashAlgo;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit init`.
#[derive(Serialize, JsonSchema)]
pub struct InitOutcome {
    /// `true` when a new repository was created.
    pub initialized: bool,
    /// `true` when an existing repository was reinitialized in place.
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub reinitialized: bool,
    /// The created `.git` directory.
    pub path: String,
    pub bare: bool,
    pub branch: String,
    /// Ref storage backend (`files` or `reftable`).
    pub ref_format: String,
    /// Object hash algorithm (`sha1` or `sha256`).
    pub object_format: String,
    /// Separate git directory when `--separate-git-dir` was used (omitted when co-located).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub git_dir: Option<String>,
}

impl HumanRender for InitOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let kind = if self.bare {
            "bare repository"
        } else {
            "repository"
        };
        if self.reinitialized {
            outln!(
                w,
                "Reinitialized existing {kind} in {} (ref-format: {})",
                self.path,
                self.ref_format
            );
        } else {
            outln!(
                w,
                "Initialized empty {kind} in {} (ref-format: {})",
                self.path,
                self.ref_format
            );
        }
        Ok(())
    }
}

impl MarkdownRender for InitOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let kind = if self.bare {
            "bare repository"
        } else {
            "repository"
        };
        outln!(
            w,
            "Initialized empty {kind} at `{}` (default branch `{}`, ref-format `{}`, object format `{}`).",
            self.path,
            self.branch,
            self.ref_format,
            self.object_format
        );
        Ok(())
    }
}

pub fn run(
    path: Option<String>,
    bare: bool,
    ref_format: RefStorageFormat,
    initial_branch: Option<String>,
    separate_git_dir: Option<PathBuf>,
    object_format: HashAlgo,
) -> Result<InitOutcome> {
    let path = PathBuf::from(path.unwrap_or_else(|| ".".to_owned()));
    let mut init_options = InitOptions::new()
        .with_bare(bare)
        .with_ref_storage(ref_format)
        .with_object_format(object_format)
        .with_environment(Environment::capture_process());
    if let Some(branch) = initial_branch {
        init_options = init_options.with_initial_branch(branch);
    }
    let separate_git_dir_for_json = separate_git_dir.clone();
    if let Some(separate) = separate_git_dir {
        init_options = init_options.with_separate_git_dir(Some(separate));
    }

    let (repo, outcome) = Repository::init_with_outcome(&path, &init_options)
        .with_context(|| format!("could not initialize a repository at {}", path.display()))?;

    Ok(InitOutcome {
        initialized: !outcome.reinitialized,
        reinitialized: outcome.reinitialized,
        path: user_display_path(repo.git_dir()),
        bare,
        branch: outcome.initial_branch,
        ref_format: ref_format.to_string(),
        object_format: object_format.name().to_owned(),
        git_dir: separate_git_dir_for_json.map(|p| user_display_path(&p)),
    })
}
