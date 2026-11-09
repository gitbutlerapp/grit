//! Divergence detection and resolution for shared branches.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;

use crate::context;
use crate::render::shared_branch::{
    DivergenceChangeJson, DivergenceJson, HeadRelation, ResolveOptionJson,
};

impl DivergenceJson {
    pub(crate) fn from_divergence(
        d: &grit_lib::divergence::Divergence,
        them: &str,
        branch: &str,
        remote: &str,
    ) -> Self {
        let options = d
            .options(them)
            .into_iter()
            .map(|o| ResolveOptionJson {
                id: o.choice.id().to_owned(),
                label: o.label,
                loses: o.loses,
                command: match o.choice {
                    grit_lib::divergence::Choice::Resolve => {
                        format!("grit rebase {remote}/shared/{branch}/{them}")
                    }
                    choice => format!("grit share resolve {branch} {} --with {them}", choice.id()),
                },
            })
            .collect();
        Self {
            options,
            scenario: d.scenario.code().to_owned(),
            summary: d.summary(them),
            needs_decision: d.scenario.needs_decision(),
            base: d.base.map(|b| b.to_hex()),
            changes: d
                .changes
                .iter()
                .map(|c| DivergenceChangeJson {
                    change: c.key.clone(),
                    subject: c.subject.clone(),
                    you: c.ours.word().to_owned(),
                    them: c.theirs.word().to_owned(),
                    paths: c.paths.clone(),
                })
                .collect(),
            conflicts: d.conflicts.iter().map(|p| p.as_str().to_owned()).collect(),
            also: d
                .scenarios
                .iter()
                .skip(1)
                .map(|s| s.code().to_owned())
                .collect(),
        }
    }
}

pub(crate) fn explain(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
) -> Option<DivergenceJson> {
    let (floor, base) = comparison(repo, remote, branch, user, ours, theirs)?;
    let d = repo
        .revwalk()
        .classify_divergence(&floor, base, ours, theirs)
        .ok()?;
    Some(DivergenceJson::from_divergence(&d, user, branch, remote))
}

/// What comparing two heads needs: the floor (the base branch's commits)
/// and the last head both had.
pub(crate) fn comparison(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
) -> Option<(Vec<ObjectId>, Option<ObjectId>)> {
    let config = ConfigSet::load(
        &context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .ok()?;
    let mut floor = Vec::new();
    if let Some(base) =
        crate::context::config_string(&config, &format!("branch.{branch}.sharedBase"))
    {
        for name in [
            format!("refs/remotes/origin/{base}"),
            format!("refs/remotes/{remote}/{base}"),
            format!("refs/heads/{base}"),
        ] {
            if let Ok(oid) = repo.references().resolve(&name) {
                if !floor.contains(&oid) {
                    floor.push(oid);
                }
            }
        }
    }
    let seen = |name: &str| {
        RefName::try_from(name)
            .map(|name| repo.revwalk().heads_seen(&name))
            .unwrap_or_default()
    };
    let ours_seen = seen(&format!("refs/heads/{branch}"));
    let theirs_seen = seen(&format!("refs/remotes/{remote}/shared/{branch}/{user}"));
    let base = repo
        .revwalk()
        .divergence_base(ours, theirs, &ours_seen, &theirs_seen)
        .ok()?;
    Some((floor, base))
}

/// Put your branch and `user`'s head back together as `choice` says,
/// without moving anything: the new head, or why not.
pub(crate) fn put_together(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
    choice: grit_lib::divergence::Choice,
) -> Result<std::result::Result<ObjectId, String>> {
    let (floor, base) =
        comparison(repo, remote, branch, user, ours, theirs).context("comparing the two heads")?;
    let made = repo
        .revwalk()
        .combine_divergence(&floor, base, ours, theirs, choice)
        .context("putting the two heads together")?;
    Ok(match made.head {
        Some(head) => Ok(head),
        None => Err(format!(
            "{} conflicts in {}",
            made.stopped_at
                .map(|s| format!("\"{s}\""))
                .unwrap_or_else(|| "a commit".to_owned()),
            made.conflicts
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    })
}

/// Move your `branch` to `head`. When it is checked out, the working tree
/// follows, keeping local changes (`grit reset --keep`, which refuses
/// rather than overwrite them).
pub(crate) fn move_branch(repo: &Repository, branch: &str, head: ObjectId) -> Result<()> {
    let checked_out = matches!(
        repo.references().head(),
        Ok(grit_lib::state::HeadState::Branch { short_name, .. }) if short_name == branch
    );
    if checked_out {
        crate::commands::reset::run(Some(head.to_hex()), false, false, false, true, false, None)
            .with_context(|| format!("moving {branch} (it is checked out)"))?;
    } else {
        repo.references()
            .update(format!("refs/heads/{branch}"), &head)
            .with_context(|| format!("moving {branch}"))?;
    }
    Ok(())
}

/// When `user`'s head and yours can be put together without losing
/// anything, and that changes yours, do it: the new head of your branch.
pub(crate) fn auto_combine(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
) -> Option<ObjectId> {
    let (floor, base) = comparison(repo, remote, branch, user, ours, theirs)?;
    let d = repo
        .revwalk()
        .classify_divergence(&floor, base, ours, theirs)
        .ok()?;
    if !d.scenario.diverged() || d.scenario.needs_decision() {
        return None;
    }
    let head = put_together(
        repo,
        remote,
        branch,
        user,
        ours,
        theirs,
        grit_lib::divergence::Choice::Combine,
    )
    .ok()?
    .ok()?;
    // Yours already has everything theirs does: nothing to do here; they
    // take yours when they sync.
    if head == ours {
        return None;
    }
    move_branch(repo, branch, head).ok()?;
    Some(head)
}
/// How another member's head `theirs` compares with your branch, `ours`.
pub(crate) fn compare(repo: &Repository, ours: Option<ObjectId>, theirs: ObjectId) -> HeadRelation {
    let Some(ours) = ours else {
        return HeadRelation::Unknown;
    };
    if ours == theirs {
        return HeadRelation::Same;
    }
    let odb = repo.odb();
    if !odb.exists(&ours) || !odb.exists(&theirs) {
        return HeadRelation::Unknown;
    }
    match (
        repo.revwalk().is_ancestor(ours, theirs).unwrap_or(false),
        repo.revwalk().is_ancestor(theirs, ours).unwrap_or(false),
    ) {
        (true, _) => HeadRelation::Ahead,
        (_, true) => HeadRelation::Behind,
        _ => HeadRelation::Diverged,
    }
}
