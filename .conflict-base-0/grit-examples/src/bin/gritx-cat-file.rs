use anyhow::{bail, Result};
use clap::Parser;
use grit_examples::resolve_name;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-cat-file",
    version,
    about = "Print an object's raw contents"
)]
struct Cli {
    object: String,
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
    let oid = resolve_name(&repo, &cli.object)?;
    let object = repo.odb().read(&oid)?;
    if object.data.contains(&0) {
        bail!("refusing to print binary object {oid}");
    }
    print!("{}", String::from_utf8_lossy(&object.data));
    Ok(())
}
