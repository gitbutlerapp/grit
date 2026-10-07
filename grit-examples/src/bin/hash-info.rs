//! Print which SHA hardware backend `grit-lib` selects on this CPU.

use clap::Parser;
use grit_lib::hash;
use grit_lib::objects::HashAlgo;

/// Report SHA-1 / SHA-256 hashing backends selected by the linked `sha1` / `sha2` crates.
#[derive(Debug, Parser)]
#[command(
    name = "hash-info",
    version,
    about = "Show grit-lib SHA hardware backends"
)]
struct Cli {
    /// Emit JSON for scripts (`{"sha1":"…","sha256":"…"}`).
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    let sha1 = hash::backend(HashAlgo::Sha1);
    let sha256 = hash::backend(HashAlgo::Sha256);
    if cli.json {
        println!(
            "{{\"sha1\":\"{}\",\"sha256\":\"{}\"}}",
            sha1.as_str(),
            sha256.as_str()
        );
    } else {
        println!("sha1 backend: {sha1}");
        println!("sha256 backend: {sha256}");
    }
}
