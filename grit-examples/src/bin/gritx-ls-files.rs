use anyhow::Result;
use clap::Parser;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(name = "gritx-ls-files", version, about = "List paths in the index")]
struct Cli;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let _cli = Cli::parse();
    let repo = Repository::discover(None)?;
    let index = repo.load_index()?;
    for entry in index.entries() {
        println!("{}", String::from_utf8_lossy(&entry.path));
    }
    Ok(())
}
