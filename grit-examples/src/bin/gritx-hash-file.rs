//! Hash a file as a Git blob (canonical object id) without touching the object database.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use grit_lib::hash;
use grit_lib::objects::{HashAlgo, ObjectKind};

/// Compute the Git blob object id for a file (same as `git hash-object` without `-w`).
#[derive(Debug, Parser)]
#[command(
    name = "gritx-hash-file",
    version,
    about = "Print the Git blob object id for a file"
)]
struct Cli {
    /// File to hash.
    file: PathBuf,
    /// Hash algorithm (default SHA-1).
    #[arg(long, value_parser = parse_algo, default_value = "sha1")]
    algo: HashAlgo,
}

fn parse_algo(s: &str) -> Result<HashAlgo, String> {
    HashAlgo::from_name(s).ok_or_else(|| format!("unknown hash algorithm {s:?}"))
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut file = std::fs::File::open(&cli.file)
        .with_context(|| format!("could not open {}", cli.file.display()))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .with_context(|| format!("could not read {}", cli.file.display()))?;
    let oid = hash::hash_object(cli.algo, ObjectKind::Blob, &buf);
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{oid}")?;
    Ok(())
}
