use anyhow::Result;
use clap::Parser;
use grit_examples::{head_oid, resolve_name};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-log",
    version,
    about = "Print a simple first-parent commit log"
)]
struct Cli {
    start: Option<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repository::discover(None, &RepositoryOptions::empty())?;
    let mut current = match cli.start {
        Some(name) => resolve_name(&repo, &name)?,
        None => head_oid(&repo)?,
    };

    loop {
        let object = repo.odb().read(&current)?;
        if object.kind != ObjectKind::Commit {
            break;
        }
        let commit = parse_commit(&object.data)?;
        println!("commit {current}");
        println!("Author: {}", commit.author);
        println!();
        for line in commit.message.lines() {
            println!("    {line}");
        }
        println!();
        let Some(parent) = commit.parents.first() else {
            break;
        };
        current = *parent;
    }
    Ok(())
}
