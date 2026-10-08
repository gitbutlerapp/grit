use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::Result;
use clap::Parser;
use grit_examples::{commit_tree, head_oid};
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-status",
    version,
    about = "Show a tiny index/work-tree status"
)]
struct Cli;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let _cli = Cli::parse();
    let repo = Repository::discover(None)?;
    let index = repo.load_index()?;
    let index_paths: BTreeSet<String> = index
        .entries()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).to_string())
        .collect();

    let head_entries = head_oid(&repo)
        .ok()
        .and_then(|oid| commit_tree(&repo, oid).ok())
        .and_then(|tree| grit_examples::entries_from_tree(&repo, tree).ok())
        .unwrap_or_default();
    let head_map: BTreeMap<String, _> = head_entries
        .into_iter()
        .map(|e| (String::from_utf8_lossy(&e.path).to_string(), e.oid))
        .collect();

    for entry in index.entries() {
        let path = String::from_utf8_lossy(&entry.path).to_string();
        match head_map.get(&path) {
            Some(oid) if oid == &entry.oid => {}
            Some(_) => println!("staged: modified {path}"),
            None => println!("staged: added {path}"),
        }
    }
    for path in head_map.keys() {
        if !index_paths.contains(path) {
            println!("staged: deleted {path}");
        }
    }

    let Some(work_tree) = repo.work_tree.as_ref() else {
        return Ok(());
    };
    for entry in index.entries() {
        let path = String::from_utf8_lossy(&entry.path).to_string();
        let full_path = work_tree.join(&path);
        if !full_path.exists() {
            println!("worktree: deleted {path}");
            continue;
        }
        let data = fs::read(&full_path)?;
        let object_id = repo.odb.write(ObjectKind::Blob, &data)?;
        if object_id != entry.oid {
            println!("worktree: modified {path}");
        }
    }
    for path in untracked_paths(work_tree, &index_paths)? {
        println!("untracked: {path}");
    }
    Ok(())
}

fn untracked_paths(work_tree: &Path, tracked: &BTreeSet<String>) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    collect_untracked(work_tree, work_tree, tracked, &mut paths)?;
    Ok(paths)
}

fn collect_untracked(
    root: &Path,
    dir: &Path,
    tracked: &BTreeSet<String>,
    paths: &mut Vec<String>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if path.is_dir() {
            collect_untracked(root, &path, tracked, paths)?;
            continue;
        }
        let rel = path.strip_prefix(root)?.to_string_lossy().to_string();
        if !tracked.contains(&rel) {
            paths.push(rel);
        }
    }
    Ok(())
}
