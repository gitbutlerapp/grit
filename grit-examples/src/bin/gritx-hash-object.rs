//! Store a file as a blob in the current repository object database.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

/// Store file contents as a Git blob and print the resulting object id.
#[derive(Debug, Parser)]
#[command(
    name = "gritx-hash-object",
    version,
    about = "Write a file to the Grit object database"
)]
struct Cli {
    /// File whose contents should be stored as a blob.
    file: PathBuf,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repository::discover(None, &RepositoryOptions::from_process())
        .context("not in a repository")?;
    let contents = std::fs::read(&cli.file)
        .with_context(|| format!("could not read {}", cli.file.display()))?;
    let oid = repo.odb().write(ObjectKind::Blob, &contents)?;
    println!("{oid}");
    Ok(())
}
