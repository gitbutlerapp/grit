use anyhow::Result;
use clap::Parser;
use grit_examples::add_path;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-add",
    version,
    about = "Stage files by writing blobs and index entries"
)]
struct Cli {
    files: Vec<std::path::PathBuf>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
    let mut index = repo.index_handle().load()?;
    for file in cli.files {
        add_path(&repo, &mut index, &file)?;
    }
    repo.index_handle().write(&mut index)?;
    Ok(())
}
