//! Cherry-pick / revert sequences that stop on conflicts (library API).
//!
//! Source for the library guide "Conflicts during replay" page.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::replay_sequence::{
    continue_replay_sequence, ReplayEmptyPolicy, ReplayOptions, SequenceOutcome,
};
use grit_lib::repo::Repository;
use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    let mut opts = RepositoryOptions::empty();
    if let Some(wt) = work_tree {
        opts = opts.with_work_tree(wt);
    }
    Repository::open(&git_dir, &opts)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;

    let opts = ReplayOptions {
        empty: ReplayEmptyPolicy::Stop,
        ..ReplayOptions::default()
    };
    println!("default empty policy: {:?}", opts.empty);

    if repo.git_dir().join("CHERRY_PICK_HEAD").exists()
        || repo.git_dir().join("REVERT_HEAD").exists()
    {
        if repo.load_index()?.has_unmerged_entries() {
            println!("unmerged index — resolve and stage before continue");
            return Ok(());
        }
        match continue_replay_sequence(&repo, "Example <ex@example.com> 1700000000 +0000")? {
            SequenceOutcome::Completed { created, .. } => {
                println!("continued; new commits={}", created.len());
            }
            SequenceOutcome::Conflicts { conflicts, .. } => {
                println!("stopped again on {} conflict(s)", conflicts.len());
            }
            SequenceOutcome::Empty { current, .. } => {
                println!("empty step at {}", current.to_hex());
            }
        }
    } else {
        println!("no replay in progress (run start_replay_sequence first)");
    }

    Ok(())
}
