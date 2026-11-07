//! Cherry-pick / revert sequences that stop on conflicts (library API).
//!
//! Source for the library guide "Conflicts during replay" page.

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::merge::{ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest, SequenceOutcome};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const IDENT: &str = "Library Guide <guide@grit-scm.com> 1735689600 +0000";

fn replay_identity() -> grit_lib::ident::Signature {
    grit_lib::ident::parse_signature(IDENT)
        .unwrap_or_else(|| panic!("guide IDENT must parse as a signature: {IDENT}"))
}

fn open_repo(root: &Path) -> Result<Repository> {
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
    Ok(Repository::open(&git_dir, &opts)?)
}

fn git(root: &Path, args: &[&str]) -> Result<()> {
    let home = root.join(".git-home");
    fs::create_dir_all(&home)?;
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(format!(
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn oid_from_rev(root: &Path, rev: &str) -> Result<ObjectId, anyhow::Error> {
    let home = root.join(".git-home");
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", rev])
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    if !out.status.success() {
        return Err(anyhow::anyhow!("rev-parse failed"));
    }
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    ObjectId::from_hex(&hex).map_err(|_| anyhow::anyhow!("bad oid"))
}

/// Build a tiny repo with one conflicting pick, run start → resolve → continue.
fn demo_conflict_sequence(root: &Path) -> Result<()> {
    git(root, &["init", "-q", "-b", "main", "."])?;
    git(root, &["config", "user.email", "guide@grit-scm.com"])?;
    git(root, &["config", "user.name", "Library Guide"])?;
    fs::write(root.join("f"), "base\n")?;
    git(root, &["add", "f"])?;
    git(root, &["commit", "-qm", "base"])?;

    git(root, &["switch", "-c", "topic"])?;
    fs::write(root.join("f"), "topic\n")?;
    git(root, &["commit", "-qam", "topic"])?;
    let pick = oid_from_rev(root, "HEAD")?;

    git(root, &["switch", "main"])?;
    fs::write(root.join("f"), "main\n")?;
    git(root, &["commit", "-qam", "main"])?;

    let repo = open_repo(root)?;
    let req = ReplaySequenceRequest {
        commits: vec![pick],
        direction: ReplayDirection::Pick,
        committer: replay_identity(),
        options: ReplayOptions {
            empty: ReplayEmptyPolicy::Stop,
            ..ReplayOptions::default()
        },
    };

    match repo.merge().start_sequence(
        &req,
        &mut NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )? {
        SequenceOutcome::Conflicts {
            conflicts,
            current,
            created_so_far,
            ..
        } => {
            println!(
                "stopped on {} conflict(s) at {}",
                conflicts.len(),
                current.to_hex()
            );
            println!("created_so_far={}", created_so_far.len());
        }
        other => {
            return Err(anyhow::anyhow!(format!(
                "expected conflicts, got {other:?}"
            )));
        }
    }

    fs::write(root.join("f"), "merged\n")?;
    repo.index().stage(
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )?;

    match repo.merge().continue_sequence(
        &replay_identity(),
        &mut NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )? {
        SequenceOutcome::Completed {
            created, dropped, ..
        } => {
            println!(
                "sequence completed; created={} dropped={}",
                created.len(),
                dropped.len()
            );
            for record in created {
                println!(
                    "replay source={} new={} change_id={}",
                    record.source.to_hex(),
                    record.new_oid.to_hex(),
                    record
                        .change_id
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".into())
                );
            }
        }
        other => {
            return Err(anyhow::anyhow!(format!(
                "expected completed after continue, got {other:?}"
            )));
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    if std::env::args().nth(1).is_none() {
        let temp = tempfile::tempdir()?;
        demo_conflict_sequence(temp.path())?;
        return Ok(());
    }

    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("missing repository path"))?;
    let repo = open_repo(&root)?;

    if repo.git_dir().join("CHERRY_PICK_HEAD").exists()
        || repo.git_dir().join("REVERT_HEAD").exists()
    {
        if repo
            .index()
            .load(&grit_lib::index::IndexLoadOptions::default())?
            .has_unmerged_entries()
        {
            println!("unmerged index — resolve and stage before continue");
            return Ok(());
        }
        match repo.merge().continue_sequence(
            &replay_identity(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )? {
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
        println!(
            "no replay in progress (omit path to run the built-in conflict demo, or call start_replay_sequence first)"
        );
    }

    Ok(())
}
