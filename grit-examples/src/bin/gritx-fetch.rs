//! `gritx-fetch` — fetch from a repository's default remote, discovering the
//! transport and authentication from the remote's configured URL.
//!
//! It reads `remote.<name>.url` (and `.fetch` refspecs) for the chosen remote —
//! an explicit argument, else the current branch's upstream, else `origin` —
//! classifies the URL (`http(s)` / `ssh` / `git://` / local), reports the auth
//! it will use (credential helpers for HTTP, SSH keys/agent for SSH, none for
//! `git://`/local), and runs the fetch in-process over grit-lib's transports.

use anyhow::Result;
use clap::Parser;
use grit_examples::remote;
use grit_lib::config::ConfigSet;
use grit_lib::repo::Repository;
use grit_lib::transfer::FetchOptions;
use grit_lib::transfer::TagMode;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-fetch",
    version,
    about = "Fetch from the default remote, auto-discovering transport + auth"
)]
struct Cli {
    /// Remote to fetch from (default: the current branch's remote, else `origin`).
    remote: Option<String>,

    /// Fetch all tags (default: follow tags pointing at fetched objects).
    #[arg(long)]
    tags: bool,

    /// Prune local remote-tracking refs that no longer exist on the remote.
    #[arg(long)]
    prune: bool,

    /// Create a shallow fetch truncated to this many commits per tip.
    #[arg(long, value_name = "N")]
    depth: Option<u32>,
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
    let git_dir = repo.git_dir.clone();
    let config = ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )?;

    let r = remote::resolve_remote(&config, &git_dir, cli.remote.as_deref(), false)?;

    eprintln!("Fetching from '{}' <{}>", r.name, r.url);
    eprintln!("  transport: {}", r.kind.label());
    eprintln!("  auth:      {}", remote::describe_auth(&config, &r));

    let opts = FetchOptions {
        refspecs: r.fetch_refspecs.clone(),
        tags: if cli.tags {
            TagMode::All
        } else {
            TagMode::Following
        },
        prune: cli.prune,
        depth: cli.depth,
        ..Default::default()
    };

    let outcome = remote::fetch(&git_dir, &r, &opts)?;

    let changed = outcome
        .updates
        .iter()
        .filter(|u| u.local_ref.is_some() && u.old_oid != u.new_oid)
        .count();
    if changed == 0 {
        eprintln!("Already up to date.");
    }
    for u in &outcome.updates {
        let Some(local) = &u.local_ref else { continue };
        if u.old_oid == u.new_oid {
            continue;
        }
        let from = u.old_oid.map(short).unwrap_or_else(|| "(new)".to_owned());
        let to = u
            .new_oid
            .map(short)
            .unwrap_or_else(|| "(deleted)".to_owned());
        println!("  {:<22} {from}..{to}  {}", format!("{:?}", u.mode), local);
    }
    if let Some(branch) = &outcome.default_branch {
        eprintln!("  remote HEAD -> {branch}");
    }
    Ok(())
}

fn short(oid: grit_lib::objects::ObjectId) -> String {
    oid.to_hex().chars().take(10).collect()
}
