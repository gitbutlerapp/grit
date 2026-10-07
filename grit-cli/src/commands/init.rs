//! `grit init` — create a new, empty repository.

use std::path::PathBuf;

use anyhow::{Context, Result};
use grit_lib::git_path::user_display_path;
use grit_lib::repo::init_repository;
use serde::Serialize;

use crate::output::HumanRender;

/// Result of `grit init`.
#[derive(Serialize)]
pub struct InitOutcome {
    pub initialized: bool,
    /// The created `.git` directory.
    pub path: String,
    pub bare: bool,
    pub branch: String,
}

impl HumanRender for InitOutcome {
    fn render_human(&self) {
        let kind = if self.bare {
            "bare repository"
        } else {
            "repository"
        };
        println!("Initialized empty {kind} in {}", self.path);
    }
}

pub fn run(path: Option<String>, bare: bool) -> Result<InitOutcome> {
    let path = PathBuf::from(path.unwrap_or_else(|| ".".to_owned()));

    let repo = init_repository(&path, bare, "main", None, "files")
        .with_context(|| format!("could not initialize a repository at {}", path.display()))?;

    Ok(InitOutcome {
        initialized: true,
        path: user_display_path(&repo.git_dir),
        bare,
        branch: "main".to_owned(),
    })
}
