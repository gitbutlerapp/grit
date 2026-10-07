// API docs: https://docs.rs/grit-lib/latest/grit_lib/ls_remote/fn.ls_remote.html
use grit_lib::ls_remote::{ls_remote, Options};
use grit_lib::objects::{parse_commit, parse_tree, tag_object_line_oid, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::refs::write_ref;
use std::collections::HashSet;
use std::env;
use std::path::PathBuf;

fn main() -> grit_lib::error::Result<()> {
    let local_git = arg_path(1, ".git");
    let remote_git = arg_path(2, "../remote.git");
    let local_odb = Odb::new(&local_git.join("objects"));
    let remote_odb = Odb::new(&remote_git.join("objects"));

    let refs = ls_remote(
        &remote_git,
        &remote_odb,
        &Options {
            heads: true,
            ..Options::default()
        },
    )?;

    for remote_ref in refs {
        copy_reachable(&remote_odb, &local_odb, remote_ref.oid)?;
        if let Some(branch) = remote_ref.name.strip_prefix("refs/heads/") {
            let tracking_ref = format!("refs/remotes/origin/{branch}");
            write_ref(&local_git, &tracking_ref, &remote_ref.oid)?;
            println!("fetched {} -> {}", remote_ref.name, tracking_ref);
        }
    }

    Ok(())
}

fn arg_path(index: usize, default: &str) -> PathBuf {
    env::args_os()
        .nth(index)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn copy_reachable(remote: &Odb, local: &Odb, root: ObjectId) -> grit_lib::error::Result<()> {
    let mut seen = HashSet::new();
    let mut stack = vec![root];

    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) || local.exists(&oid) {
            continue;
        }

        let object = remote.read(&oid)?;
        local.write(object.kind, &object.data)?;

        match object.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&object.data)?;
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            ObjectKind::Tree => {
                stack.extend(parse_tree(&object.data)?.into_iter().map(|entry| entry.oid));
            }
            ObjectKind::Tag => {
                if let Some(target) = tag_object_line_oid(&object.data) {
                    stack.push(target);
                }
            }
            ObjectKind::Blob => {}
        }
    }

    Ok(())
}
