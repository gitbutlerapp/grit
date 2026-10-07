//! `grit commit` — stage every change and record a new commit.

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::state::HeadState;
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use serde::Serialize;
use time::OffsetDateTime;

use crate::commands::add;
use crate::context::{self, subject_line};
use crate::output::HumanRender;

/// Result of `grit commit`.
#[derive(Serialize)]
pub struct CommitOutcome {
    pub oid: String,
    pub branch: String,
    pub subject: String,
    pub changes: usize,
}

impl HumanRender for CommitOutcome {
    fn render_human(&self) {
        println!(
            "[{} {}] {}",
            self.branch,
            self.oid.get(..7).unwrap_or(&self.oid),
            self.subject
        );
        println!(
            "{} change{} committed",
            self.changes,
            if self.changes == 1 { "" } else { "s" }
        );
    }
}

pub fn run(message: Option<String>) -> Result<CommitOutcome> {
    let repo = context::discover()?;

    add::stage(&repo, &[])?;

    let message = match message {
        Some(m) if !m.trim().is_empty() => m,
        _ => bail!("provide a commit message, e.g. grit commit \"what changed\""),
    };

    let model = status(&repo, &StatusOptions::default(), &mut NullProgress)
        .context("could not compute status")?;
    if model.staged.is_empty() {
        bail!("nothing to commit — working tree clean");
    }

    let (refname, short_name, parent) = match &model.head {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname.clone(), short_name.clone(), *oid),
        HeadState::Detached { .. } => {
            bail!("HEAD is detached; grit commit needs a branch")
        }
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    };

    let mut index = model.index;
    let tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())
        .context("could not write tree")?;
    repo.write_index(&mut index)
        .context("could not refresh index cache-tree")?;

    let config = ConfigSet::load(Some(&repo.git_dir), true).context("could not load config")?;
    let now = OffsetDateTime::now_utc();
    let author = context::identity(&config, IdentRole::Author, "GIT_AUTHOR_DATE", now)?;
    let committer = context::identity(&config, IdentRole::Committer, "GIT_COMMITTER_DATE", now)?;

    let mut message = message.trim().to_owned();
    message.push('\n');
    let subject = subject_line(&message);

    let commit_data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author,
        committer: committer.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message,
        raw_message: None,
    };
    let bytes = serialize_commit(&commit_data);
    let oid = repo
        .odb
        .write(ObjectKind::Commit, &bytes)
        .context("could not store commit")?;

    let old = parent.unwrap_or_else(ObjectId::zero);
    let reflog_msg = if parent.is_some() {
        format!("commit: {subject}")
    } else {
        format!("commit (initial): {subject}")
    };
    refs::write_ref(&repo.git_dir, &refname, &oid).context("could not update branch")?;
    refs::append_reflog(
        &repo.git_dir,
        &refname,
        &old,
        &oid,
        &committer,
        &reflog_msg,
        false,
    )?;
    refs::append_reflog(
        &repo.git_dir,
        "HEAD",
        &old,
        &oid,
        &committer,
        &reflog_msg,
        false,
    )?;

    let count = model.staged.len();
    Ok(CommitOutcome {
        oid: oid.to_hex(),
        branch: short_name,
        subject,
        changes: count,
    })
}
