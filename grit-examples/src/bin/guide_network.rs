//! List refs on a local remote, fetch, create a commit, and push over `file://`.
//!
//! Source for the library guide "Network" page (included in the docs site).

use grit_examples::remote;
use grit_lib::config::ConfigSet;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::remote::{list_refs_from_git_dir, ListRefsOptions};
use grit_lib::repo::Repository;
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::transport_path::resolve_local_remote_git_dir;
use std::path::Path;

fn main() -> Result<(), grit_lib::error::Error> {
    let consumer = std::env::args().nth(1).ok_or_else(|| {
        grit_lib::error::Error::Message("usage: guide_network <consumer-repo>".to_owned())
    })?;
    let consumer = Path::new(&consumer);
    let repo = Repository::discover(Some(consumer))?;
    let git_dir = repo.git_dir.clone();
    let work_tree = repo.work_tree.clone();
    let config = ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )?;

    let remote_info = remote::resolve_remote(&config, &git_dir, Some("origin"), false)
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    let remote_git_dir =
        resolve_local_remote_git_dir(&remote_info.url, &git_dir, work_tree.as_deref());
    let remote_repo = Repository::open(&remote_git_dir, None)?;
    let refs_on_remote = list_refs_from_git_dir(
        &remote_git_dir,
        &remote_repo.odb,
        &ListRefsOptions::default(),
    )
    .map_err(grit_lib::error::Error::from)?;
    eprintln!(
        "ls-remote: {} ref(s) on {}",
        refs_on_remote.len(),
        remote_git_dir.display()
    );

    let fetch_opts = FetchOptions {
        refspecs: remote_info.fetch_refspecs.clone(),
        tags: TagMode::Following,
        ..Default::default()
    };
    remote::fetch(&git_dir, &remote_info, &fetch_opts)
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    let tracking = refs::resolve_ref(&git_dir, "refs/remotes/origin/main")?;
    refs::write_ref(&git_dir, "refs/heads/main", &tracking)?;

    let parent_obj = repo.odb.read(&tracking)?;
    let parent = parse_commit(&parent_obj.data)?;
    let commit = CommitData {
        tree: parent.tree,
        parents: vec![tracking],
        author: parent.author.clone(),
        committer: parent.committer.clone(),
        author_raw: parent.author_raw.clone(),
        committer_raw: parent.committer_raw.clone(),
        encoding: parent.encoding.clone(),
        message: "library guide network example\n".to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let new_oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    refs::write_ref(&git_dir, "refs/heads/main", &new_oid)?;

    let spec = PushRefSpec {
        src: Some(new_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    remote::push(&git_dir, &remote_info, &[spec], &PushOptions::default())
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    println!("{new_oid}");
    Ok(())
}
