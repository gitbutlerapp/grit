use anyhow::Result;
use clap::Parser;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::ident::Signature;
use grit_lib::objects::NewCommit;
use grit_lib::repo::Repository;
use time::OffsetDateTime;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-commit-tree",
    version,
    about = "Create a commit object from a tree"
)]
struct Cli {
    tree: String,
    #[arg(short = 'p', long = "parent")]
    parents: Vec<String>,
    #[arg(short = 'm', long = "message", default_value = "example commit")]
    message: String,
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
    let tree = cli.tree.parse()?;
    let parents = cli
        .parents
        .iter()
        .map(|p| p.parse())
        .collect::<Result<Vec<_>, _>>()?;
    let when = OffsetDateTime::from_unix_timestamp(1_700_000_000)?;
    let ident = Signature::new("Grit Example", "grit@example.invalid", when)?;
    let oid = repo.objects().write_commit(
        &NewCommit::new(tree, ident.clone(), ident, cli.message).with_parents(parents),
    )?;
    println!("{oid}");
    Ok(())
}
