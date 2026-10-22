use anyhow::Result;
use clap::Parser;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-for-each-ref",
    version,
    about = "List refs and object ids"
)]
struct Cli {
    #[arg(default_value = "refs/")]
    prefix: String,
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
    for (name, oid) in refs::list_refs(repo.git_dir(), &cli.prefix)? {
        println!("{oid} {name}");
    }
    Ok(())
}
