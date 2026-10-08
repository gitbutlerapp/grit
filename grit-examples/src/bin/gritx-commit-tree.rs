use anyhow::Result;
use clap::Parser;
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::repo::Repository;

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
    let repo = Repository::discover(None)?;
    let tree = cli.tree.parse()?;
    let parents = cli
        .parents
        .iter()
        .map(|p| p.parse())
        .collect::<Result<Vec<_>, _>>()?;
    let ident = "Grit Example <grit@example.invalid> 1700000000 +0000".to_owned();
    let commit = CommitData {
        tree,
        parents,
        author: ident.clone(),
        committer: ident,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{}\n", cli.message),
        raw_message: None,
        preserved_preamble: Vec::new(),
    };
    let oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    println!("{oid}");
    Ok(())
}
