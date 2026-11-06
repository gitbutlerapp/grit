//! List refs on a local remote, fetch, create a commit, and push over `file://`.
//!
//! Source for the library guide "Network" page (included in the docs site).

use anyhow::Result;
use grit_examples::remote;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::ident::parse_signature;
use grit_lib::objects::NewCommit;
use grit_lib::refs;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::remote::transport_path::resolve_local_remote_git_dir;
use grit_lib::remote::{list_refs_from_git_dir, ListRefsOptions};
use grit_lib::repo::Repository;
use std::path::Path;

fn main() -> Result<()> {
    let consumer = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: guide_network <consumer-repo>"))?;
    let consumer = Path::new(&consumer);
    let repo = Repository::discover(Some(consumer), &RepositoryOptions::from_process())?;
    let git_dir = repo.git_dir().to_path_buf();
    let work_tree = repo.work_tree().map(|p| p.to_path_buf());
    let config = ConfigSet::load(
        &grit_lib::config::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )?;

    let remote_info = remote::resolve_remote(&config, &git_dir, Some("origin"), false)?;

    let remote_git_dir =
        resolve_local_remote_git_dir(&remote_info.url, &git_dir, work_tree.as_deref());
    let remote_repo = Repository::open(&remote_git_dir, &RepositoryOptions::from_process())?;
    let refs_on_remote = list_refs_from_git_dir(
        &remote_git_dir,
        Some(remote_repo.odb()),
        &ListRefsOptions::default(),
    )?;
    eprintln!(
        "ls-remote: {} ref(s) on {}",
        refs_on_remote.len(),
        remote_git_dir.display()
    );

    let mut fetch_opts = FetchOptions::new();
    fetch_opts.refspecs = remote_info.fetch_refspecs.clone();
    fetch_opts.tags = TagMode::Following;
    remote::fetch(&repo, &remote_info, &fetch_opts)?;

    let tracking = refs::resolve_ref(&git_dir, "refs/remotes/origin/main")?;
    refs::write_ref(&git_dir, "refs/heads/main", &tracking)?;

    let parent = repo.objects().read_commit(&tracking)?;
    let committer = parse_signature(&parent.committer)
        .ok_or_else(|| anyhow::anyhow!("parent committer is not a valid identity"))?;
    let new_oid = repo.objects().write_commit(
        &NewCommit::new(
            parent.tree,
            committer.clone(),
            committer,
            "library guide network example",
        )
        .with_parents([tracking]),
    )?;
    refs::write_ref(&git_dir, "refs/heads/main", &new_oid)?;

    let spec = PushRefSpec {
        src: Some(new_oid),
        src_ref: Some("refs/heads/main".to_owned()),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
        hook_src_ref: None,
    };
    remote::push(&repo, &remote_info, &[spec], &PushOptions::default())?;

    println!("{new_oid}");
    Ok(())
}
