//! `grit init` — create a new, empty repository.

use std::path::PathBuf;

use anyhow::{Context, Result};
use grit_lib::git_path::user_display_path;
use grit_lib::ref_storage::RefStorageFormat;
use grit_lib::repo::init_repository;
use serde::Serialize;

use crate::output::{HumanRender, MarkdownRender};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit init`.
#[derive(Serialize)]
pub struct InitOutcome {
    /// `true` when a new repository was created.
    pub initialized: bool,
    /// `true` when an existing repository was reinitialized in place.
    #[serde(skip_serializing_if = "is_false")]
    pub reinitialized: bool,
    /// The created `.git` directory.
    pub path: String,
    pub bare: bool,
    pub branch: String,
    /// Ref storage backend (`files` or `reftable`).
    pub ref_format: String,
}

impl HumanRender for InitOutcome {
    fn render_human(&self) {
        let kind = if self.bare {
            "bare repository"
        } else {
            "repository"
        };
        if self.reinitialized {
            println!(
                "Reinitialized existing {kind} in {} (ref-format: {})",
                self.path, self.ref_format
            );
        } else {
            println!(
                "Initialized empty {kind} in {} (ref-format: {})",
                self.path, self.ref_format
            );
        }
    }
}

impl MarkdownRender for InitOutcome {
    fn render_markdown(&self) {
        let kind = if self.bare {
            "bare repository"
        } else {
            "repository"
        };
        println!(
            "Initialized empty {kind} at `{}` (default branch `{}`, ref-format `{}`).",
            self.path, self.branch, self.ref_format
        );
    }
}

pub fn run(path: Option<String>, bare: bool, ref_format: RefStorageFormat) -> Result<InitOutcome> {
    let path = PathBuf::from(path.unwrap_or_else(|| ".".to_owned()));
    let git_dir = if bare {
        path.clone()
    } else {
        path.join(".git")
    };
    let reinitialized = git_dir.join("config").is_file();

    let repo = init_repository(&path, bare, "main", None, ref_format)
        .with_context(|| format!("could not initialize a repository at {}", path.display()))?;

    Ok(InitOutcome {
        initialized: !reinitialized,
        reinitialized,
        path: user_display_path(&repo.git_dir),
        bare,
        branch: "main".to_owned(),
        ref_format: ref_format.to_string(),
    })
}
