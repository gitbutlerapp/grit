//! Verify a signed tag's SSH/OpenPGP signature and optional `tree-sha256` header.
//!
//! Source for the library guide "Tree SHA-256" verification section.

use grit_lib::commit::signing::GpgConfig;
use grit_lib::error::Error;
use grit_lib::tree_sha256::TagVerifyOptions;

fn main() -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(Error::Io)?;
    let env = grit_lib::config::environment::Environment::from_vars(std::env::vars_os(), cwd);
    let options = grit_lib::config::environment::RepositoryOptions::with_environment(env);
    let repo = grit_lib::repo::Repository::discover(None, &options)?;
    let tag = std::env::args()
        .nth(1)
        .ok_or_else(|| Error::Message("usage: guide_tree_sha256_verify <tag>".to_owned()))?;
    let oid = grit_lib::revwalk::rev_parse::resolve_revision(&repo, &tag)?;
    let gpg =
        GpgConfig::from_config(repo.config()?.as_ref())?.with_command_runner(repo.command_runner());
    let report = repo.verify_tag(
        oid,
        &TagVerifyOptions {
            check_signature: true,
            gpg,
        },
    )?;
    println!("signature: {:?}", report.signature);
    if let Some(v) = &report.tree_sha256 {
        println!("claimed: {}", v.claimed);
        println!("computed: {}", v.computed);
    }
    if report.is_ok() {
        println!("OK");
    } else {
        std::process::exit(1);
    }
    Ok(())
}
