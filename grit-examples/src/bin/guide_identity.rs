//! Resolve author identity and build a [`CommitRequest`](grit_lib::porcelain::commit::CommitRequest).
//!
//! Source for the library guide "Repository" page Identity section (included in the docs site).

use std::ffi::OsString;

use grit_lib::config::environment::Environment;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::porcelain::commit::CommitRequest;
use grit_lib::repo::{InitOptions, Repository};

fn main() -> Result<(), Error> {
    let base = tempfile::tempdir().map_err(Error::Io)?;
    let root = base.path().join("repo");

    let env = Environment::from_vars(
        [
            ("GIT_AUTHOR_NAME", OsString::from("Ada Lovelace")),
            ("GIT_AUTHOR_EMAIL", OsString::from("ada@example.com")),
            ("GIT_COMMITTER_NAME", OsString::from("Committer Example")),
            (
                "GIT_COMMITTER_EMAIL",
                OsString::from("committer@example.com"),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v)),
        root.clone(),
    );

    let repo = Repository::init(
        &root,
        &InitOptions::new()
            .with_initial_branch("main")
            .with_environment(env),
    )?;

    let author = repo.identity(IdentRole::Author, repo.identity_now())?;
    let mut request = CommitRequest::new("first\n");
    request.author = Some(author);
    // committer left None — create_commit uses the repository committer identity

    if let Some(author) = &request.author {
        println!("author={}", author.name);
    }

    Ok(())
}
