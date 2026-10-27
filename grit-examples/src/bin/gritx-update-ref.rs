use anyhow::Result;
use clap::Parser;
use grit_examples::resolve_name;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(name = "gritx-update-ref", version, about = "Point a ref at an object")]
struct Cli {
    refname: String,
    new_value: String,
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
    let oid = resolve_name(&repo, &cli.new_value)?;
    refs::write_ref(repo.git_dir(), &cli.refname, &oid)?;
    Ok(())
}
