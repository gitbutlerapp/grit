//! A minimal Git server on stdin/stdout: serve fetches with `upload_pack` and
//! pushes with `receive_pack_with_hook`, refusing pushes to protected branches
//! from Rust code instead of a hook script.
//!
//! Git starts it with the repository path as the last argument:
//!
//! ```text
//! git clone --upload-pack='guide_serve upload-pack' /srv/repo.git
//! git push --receive-pack='guide_serve receive-pack' /srv/repo.git main
//! ```
//!
//! Source for the library guide "Network" page (included in the docs site).

use std::io::{BufWriter, Write};
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::Odb;
use grit_lib::remote::serve::{
    self, ProtocolVersion, ReceivePolicy, RefCommand, RefUpdateRefusal, ServeOptions,
};
use grit_lib::repo::Repository;

/// Refuse every update to `refs/heads/protected/*`; the reason is shown to the
/// pushing user next to the rejected ref.
fn protect_branches(cmd: &RefCommand, _odb: &Odb) -> Result<(), RefUpdateRefusal> {
    if cmd.refname.starts_with("refs/heads/protected/") {
        return Err(RefUpdateRefusal::Policy("protected branch".to_owned()));
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(service), Some(path)) = (args.next(), args.next()) else {
        return Err("usage: guide_serve <upload-pack|receive-pack> <repository>".into());
    };
    let repo = Repository::open(Path::new(&path), &RepositoryOptions::empty().for_serving())?;

    // The process boundary owns the environment: the library never reads it.
    let protocol = std::env::var("GIT_PROTOCOL")
        .map(|v| ProtocolVersion::from_git_protocol(&v))
        .unwrap_or_default();
    let mut opts = ServeOptions {
        protocol,
        agent: "guide-serve/1".to_owned(),
        ..ServeOptions::default()
    };

    let mut input = std::io::stdin().lock();
    let mut output = BufWriter::new(std::io::stdout().lock());
    match service.as_str() {
        "upload-pack" => serve::upload_pack(&repo, &mut input, &mut output, &mut opts)?,
        "receive-pack" => {
            let policy = ReceivePolicy::from_config(&*repo.config()?);
            let outcome = serve::receive_pack_with_hook(
                &repo,
                &mut input,
                &mut output,
                &opts,
                &policy,
                &protect_branches,
            )?;
            // Typed results: match on the refusal, never on its text.
            for update in &outcome.updates {
                if let Some(RefUpdateRefusal::Policy(reason)) = &update.error {
                    eprintln!("refused {}: {reason}", update.refname);
                }
            }
        }
        other => return Err(format!("unknown service '{other}'").into()),
    }
    output.flush()?;
    Ok(())
}
