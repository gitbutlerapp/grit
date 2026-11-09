//! Share command operations (`add`, `list`, `accept`, …).

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::remote::grit_server_auth::{ServerAuthError, ServerUrl};
use grit_lib::remote::grit_server_share::{
    self as api, LinkOptions, LinkUses, ShareAcceptRequest, ShareAcceptStatus, ShareMode,
    ShareReply, ShareRequest, ShareState, SharedBranch,
};
use grit_lib::remote::HttpClientFactory as _;
use grit_lib::repo::Repository;

use crate::commands::sync::{configure_shared, ensure_space, fetch, push, tips};
use crate::context;
use crate::dispatch::CommandInteraction;
use crate::render::share::{
    HeadRelation, ShareAcceptOutcome, ShareBranchJson, ShareLinkJson, ShareMemberJson,
    ShareOutcome, ShareResolveOutcome,
};

use super::divergence::{compare, explain, move_branch, put_together};
use super::integrated::{integrated_report, resolve_integrated, INTEGRATED_OPTIONS};

fn state_word(state: ShareState) -> &'static str {
    match state {
        ShareState::Active => "active",
        ShareState::Integrated => "integrated",
        ShareState::Ended => "ended",
    }
}

fn mode_word(mode: ShareMode) -> &'static str {
    match mode {
        ShareMode::Full => "full",
        ShareMode::Snapshot => "snapshot",
    }
}

fn relation(repo: &Repository, ours: Option<ObjectId>, theirs: &str) -> HeadRelation {
    match theirs.parse::<ObjectId>() {
        Ok(theirs) => compare(repo, ours, theirs),
        Err(_) => HeadRelation::Unknown,
    }
}
fn branch_json(here: Option<(&Repository, &str)>, share: SharedBranch) -> ShareBranchJson {
    let repo = here.map(|(repo, _)| repo);
    let ours = repo.and_then(|repo| {
        tips(repo, "refs/heads/")
            .ok()
            .and_then(|t| t.get(&share.branch).copied())
    });
    let branch = share.branch.clone();
    ShareBranchJson {
        members: share
            .members
            .into_iter()
            .map(|m| {
                let relation = match (repo, &m.head, m.you) {
                    (Some(repo), Some(head), false) => Some(relation(repo, ours, head)),
                    _ => None,
                };
                let divergence = match (here, relation, ours, m.head.as_deref()) {
                    (
                        Some((repo, remote)),
                        Some(HeadRelation::Diverged),
                        Some(ours),
                        Some(head),
                    ) => head
                        .parse::<ObjectId>()
                        .ok()
                        .and_then(|head| explain(repo, remote, &branch, &m.user, ours, head)),
                    _ => None,
                };
                (m, relation, divergence)
            })
            .map(|(m, relation, divergence)| ShareMemberJson {
                relation,
                divergence,
                user: m.user,
                sharer: m.sharer,
                you: m.you,
                head: m.head,
                project: m.project,
                snapshot: m.view.map(|v| v.paths),
            })
            .collect(),
        links: share
            .links
            .into_iter()
            .map(|l| ShareLinkJson {
                id: l.id,
                url: None,
                uses_left: l.uses_left,
                expires_at: l.expires_at,
                snapshot: l.view.map(|v| v.paths),
            })
            .collect(),
        invited: share.invited,
        branch: share.branch,
        origin: share.origin,
        sharer: share.sharer,
        state: state_word(share.state).to_owned(),
        mode: mode_word(share.mode).to_owned(),
        base_branch: share.base_branch,
        url: share.url,
    }
}

fn server_error(server: &ServerUrl, e: ServerAuthError) -> anyhow::Error {
    match e {
        ServerAuthError::TokenRejected => anyhow::anyhow!(
            "{} needs you signed in: run `grit login {}`",
            server.host(),
            server.root_url()
        ),
        ServerAuthError::NotGritServer { .. } => anyhow::anyhow!(
            "{} doesn't share branches: sharing needs a grit hosting server such as grithub",
            server.host()
        ),
        other => anyhow::anyhow!("{other}"),
    }
}

/// Ask `server` about shares in `project`.
pub(crate) fn ask_about(
    config: &ConfigSet,
    server: &ServerUrl,
    project: &str,
    request: &ShareRequest,
) -> Result<ShareReply> {
    let http = context::http_client_factory()
        .create(config)
        .context("could not set up HTTP client")?;
    let reply =
        api::share(http.as_ref(), server, project, request).map_err(|e| server_error(server, e))?;
    if let Some(why) = &reply.refused {
        bail!("{why}");
    }
    Ok(reply)
}

/// The clone, set up against your space.
pub(crate) struct Here {
    pub(crate) repo: Repository,
    pub(crate) config: ConfigSet,
    pub(crate) server: ServerUrl,
    pub(crate) project: String,
    pub(crate) remote: String,
}

fn here() -> Result<Here> {
    let repo = context::discover()?;
    if crate::commands::checkout::open_checkout_here()?.is_some() {
        bail!("this is a snapshot checkout: share branches from a clone of the project");
    }
    let config = ConfigSet::load(
        &context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .context("could not load config")?;
    let Some(setup) = ensure_space(&repo, &config, "origin")? else {
        bail!("origin is not a project on a grit hosting server; sharing needs a clone of one, such as `grit clone https://grithub.dev/<owner>/<project>`");
    };
    let config = ConfigSet::load(
        &context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .context("could not load config")?;
    Ok(Here {
        repo,
        config,
        server: setup.server,
        project: setup.space.project,
        remote: setup.remote,
    })
}
fn ask(here: &Here, request: &ShareRequest) -> Result<ShareReply> {
    ask_about(&here.config, &here.server, &here.project, request)
}

/// Bring the shares' heads into the clone after a change.
fn refresh(here: &Here, reply: &ShareReply, interaction: CommandInteraction) -> Result<()> {
    for share in &reply.shares {
        configure_shared(
            &here.repo,
            &here.remote,
            std::slice::from_ref(&share.branch),
            share.base_branch.as_deref(),
        )?;
    }
    fetch(&here.remote, interaction.human_progress)
}

fn outcome(
    here: &Here,
    action: &str,
    reply: ShareReply,
    pushed: bool,
    list_branch: Option<&str>,
) -> ShareOutcome {
    let shares = api::filter_listed_shares(reply.shares, list_branch);
    ShareOutcome {
        action: action.to_owned(),
        project: reply.project,
        link: reply.link.map(|l| ShareLinkJson {
            id: l.info.id,
            url: Some(l.url),
            uses_left: l.info.uses_left,
            expires_at: l.info.expires_at,
            snapshot: l.info.view.map(|v| v.paths),
        }),
        shares: shares
            .into_iter()
            .map(|s| branch_json(Some((&here.repo, here.remote.as_str())), s))
            .collect(),
        added: reply.added,
        removed: reply.removed,
        pushed,
    }
}

/// Parse `--uses`: a number, or `unlimited`.
pub fn parse_uses(text: &str) -> Result<LinkUses> {
    if text.eq_ignore_ascii_case("unlimited") {
        return Ok(LinkUses::Unlimited);
    }
    match text.parse::<u32>() {
        Ok(0) | Err(_) => bail!("--uses takes a positive number or `unlimited`, not {text:?}"),
        Ok(n) => Ok(LinkUses::Count(n)),
    }
}

/// `grit share add`.
pub fn add(
    branch: &str,
    users: &[String],
    link: bool,
    uses: Option<&str>,
    expires: Option<u32>,
    view: Option<api::ShareView>,
    interaction: CommandInteraction,
) -> Result<ShareOutcome> {
    let human_progress = interaction.human_progress;
    if !link && (uses.is_some() || expires.is_some()) {
        bail!("--uses and --expires go with --link");
    }
    if !link && users.is_empty() {
        bail!("name someone to share {branch} with, or pass --link to make a link anyone signed in can accept");
    }
    let here = here()?;
    let local = tips(&here.repo, "refs/heads/")?.get(branch).copied();
    let remote = tips(&here.repo, &format!("refs/remotes/{}/", here.remote))?
        .get(branch)
        .copied();
    let pushed = match (local, remote) {
        (None, None) => bail!("there is no branch {branch} here or in your space"),
        (Some(local), remote) if Some(local) != remote => {
            if let Some(Some(why)) =
                push(&here.remote, &[branch.to_owned()], human_progress)?.get(branch)
            {
                bail!("pushing {branch} to your space first: {why}");
            }
            true
        }
        _ => false,
    };
    let options = match link {
        true => Some(LinkOptions {
            uses: uses.map(parse_uses).transpose()?.unwrap_or_default(),
            expires_days: expires.unwrap_or(api::DEFAULT_LINK_DAYS),
        }),
        false => None,
    };
    if let Some(days) = expires {
        if !(1..=api::MAX_LINK_DAYS).contains(&days) {
            bail!("--expires takes 1 to {} days", api::MAX_LINK_DAYS);
        }
    }
    let reply = ask(
        &here,
        &ShareRequest::Share {
            branch: branch.to_owned(),
            users: users.to_vec(),
            link: options,
            view,
        },
    )?;
    refresh(&here, &reply, interaction)?;
    Ok(outcome(&here, "add", reply, pushed, None))
}

/// `grit share list`.
pub fn list(branch: Option<&str>, interaction: CommandInteraction) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::List {
            branch: branch.map(str::to_owned),
        },
    )?;
    if !reply.shares.is_empty() {
        refresh(&here, &reply, interaction)?;
    }
    Ok(outcome(&here, "list", reply, false, branch))
}

/// `grit share remove`.
pub fn remove(
    branch: &str,
    users: &[String],
    interaction: CommandInteraction,
) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::Unshare {
            branch: branch.to_owned(),
            users: users.to_vec(),
        },
    )?;
    if users.is_empty() {
        configure_shared(&here.repo, &here.remote, &[], None)?;
    }
    fetch(&here.remote, interaction.human_progress)?;
    Ok(outcome(&here, "remove", reply, false, None))
}

/// `grit share leave`.
pub fn leave(branch: &str, interaction: CommandInteraction) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::Leave {
            branch: branch.to_owned(),
        },
    )?;
    crate::commands::sync::unmark_shared(&here.repo, branch)?;
    fetch(&here.remote, interaction.human_progress)?;
    Ok(outcome(&here, "leave", reply, false, None))
}

/// `grit share revoke`.
pub fn revoke(branch: &str, link: &str) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::RevokeLink {
            branch: branch.to_owned(),
            link: link.to_owned(),
        },
    )?;
    Ok(outcome(&here, "revoke", reply, false, None))
}
pub fn resolve(
    branch: &str,
    option: Option<&str>,
    name: Option<&str>,
    with: Option<&str>,
    interaction: CommandInteraction,
) -> Result<ShareResolveOutcome> {
    let here = here()?;
    fetch(&here.remote, interaction.human_progress)?;
    let Some(ours) = tips(&here.repo, "refs/heads/")?.get(branch).copied() else {
        bail!("there is no branch {branch} here");
    };
    let heads = crate::commands::sync::shared_heads(&here.repo, &here.remote, branch)?;
    let integrated_option = option.is_some_and(|o| INTEGRATED_OPTIONS.contains(&o));
    if heads.is_empty() || integrated_option {
        if let Some(report) = integrated_report(&here, branch, ours, !integrated_option)? {
            return resolve_integrated(&here, branch, ours, report, option, name, interaction);
        }
        if integrated_option {
            bail!(
                "{branch} wasn't shared and integrated lately, so there's nothing to {}",
                option.unwrap_or_default()
            );
        }
        bail!("{branch} isn't shared here, or nobody else has pushed a head of it yet");
    }
    let diverged: Vec<(String, ObjectId)> = heads
        .into_iter()
        .filter(|(user, tip)| {
            with.is_none_or(|w| w == user)
                && compare(&here.repo, Some(ours), *tip) == HeadRelation::Diverged
        })
        .collect();
    let (user, theirs) = match (diverged.as_slice(), with) {
        ([one], _) => one.clone(),
        ([], Some(w)) => {
            bail!("{w}'s head of {branch} hasn't diverged from yours: `grit sync` brings it in")
        }
        ([], None) => {
            bail!("nobody's head of {branch} has diverged from yours: `grit sync` brings theirs in")
        }
        (many, None) => bail!(
            "several heads of {branch} diverged from yours ({}): name one with --with <user>",
            many.iter()
                .map(|(u, _)| u.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        (_, Some(w)) => bail!("{w} isn't on {branch}"),
    };
    let report = explain(&here.repo, &here.remote, branch, &user, ours, theirs)
        .context("comparing the two heads")?;
    let mut outcome = ShareResolveOutcome {
        branch: branch.to_owned(),
        with: user.clone(),
        scenario: report.scenario.clone(),
        summary: report.summary.clone(),
        option: None,
        previous: ours.to_hex(),
        head: None,
        pushed: false,
        next: None,
        options: Vec::new(),
        renamed: None,
        shared_with: Vec::new(),
    };
    let chosen = match option {
        Some(id) => id.to_owned(),
        None if !report.needs_decision => "combine".to_owned(),
        None => {
            outcome.options = report.options;
            return Ok(outcome);
        }
    };
    let Some(choice) = grit_lib::divergence::Choice::parse(&chosen) else {
        let ids: Vec<&str> = report.options.iter().map(|o| o.id.as_str()).collect();
        bail!(
            "{chosen:?} isn't an option here; choose one of: {}",
            ids.join(", ")
        );
    };
    if !report.options.iter().any(|o| o.id == chosen) {
        let ids: Vec<&str> = report.options.iter().map(|o| o.id.as_str()).collect();
        bail!(
            "{chosen} isn't an option for {} ({}); choose one of: {}",
            report.scenario,
            report.summary,
            ids.join(", ")
        );
    }
    outcome.option = Some(chosen);
    if choice == grit_lib::divergence::Choice::Resolve {
        outcome.next = Some(format!(
            "grit rebase {}/shared/{branch}/{user}, settle the conflicts and `grit rebase --continue`, then `grit push`",
            here.remote
        ));
        return Ok(outcome);
    }
    let head = match put_together(&here.repo, &here.remote, branch, &user, ours, theirs, choice)? {
        Ok(head) => head,
        Err(why) => bail!(
            "{why}: choose another option, or resolve it by hand with `grit share resolve {branch} resolve --with {user}`"
        ),
    };
    if head != ours {
        move_branch(&here.repo, branch, head)?;
    }
    outcome.head = Some(head.to_hex());
    if let Some(Some(why)) = push(
        &here.remote,
        &[branch.to_owned()],
        interaction.human_progress,
    )?
    .get(branch)
    {
        bail!(
            "{branch} is at {} here, but pushing it was refused: {why}",
            short_hex(&head.to_hex())
        );
    }
    outcome.pushed = true;
    Ok(outcome)
}
pub fn accept(link: &str, interaction: CommandInteraction) -> Result<ShareAcceptOutcome> {
    let Some((server, _token)) = api::share_link(link) else {
        bail!("{link} isn't a share link: they look like https://grithub.dev/share/<token>");
    };
    let repo = context::discover().ok();
    let env = context::environment();
    let config = ConfigSet::load(
        &env,
        repo.as_ref().map(Repository::git_dir),
        &grit_lib::config::LoadConfigOptions::standard(
            &env,
            repo.as_ref().map(Repository::git_dir),
        ),
    )
    .context("could not load config")?;
    let http = context::http_client_factory()
        .create(&config)
        .context("could not set up HTTP client")?;
    let reply = api::accept(
        http.as_ref(),
        &server,
        &ShareAcceptRequest {
            link: link.to_owned(),
        },
    )
    .map_err(|e| server_error(&server, e))?;
    if reply.status == ShareAcceptStatus::Refused {
        bail!(
            "{}",
            reply
                .message
                .unwrap_or_else(|| "the link can't be accepted".to_owned())
        );
    }
    let (Some(share), Some(project), Some(url)) = (reply.share, reply.project, reply.url) else {
        bail!("{} answered without the share", server.host());
    };
    let status = match reply.status {
        ShareAcceptStatus::AlreadyMember => "already_member",
        _ => "accepted",
    };
    // In a clone of the project (or of the organization's project it
    // copies), fetch into it; anywhere else, clone your copy.
    let in_clone = repo.as_ref().and_then(|repo| {
        let url = crate::context::config_string(&config, "remote.origin.url")?;
        let (origin_server, origin_project) =
            grit_lib::remote::grit_server_space::hosted_project(&url)?;
        let name = |p: &str| p.split_once('/').map(|(_, n)| n.to_owned());
        (origin_server.root_url() == server.root_url() && name(&origin_project) == name(&project))
            .then_some(repo)
    });
    let (path, cloned) = match in_clone {
        Some(repo) => {
            let config = ConfigSet::load(
                &context::environment(),
                Some(repo.git_dir()),
                &grit_lib::config::LoadConfigOptions::standard(
                    &context::environment(),
                    Some(repo.git_dir()),
                ),
            )
            .context("could not load config")?;
            let Some(setup) = ensure_space(repo, &config, "origin")? else {
                bail!("origin is not a project on {}", server.host());
            };
            configure_shared(
                repo,
                &setup.remote,
                std::slice::from_ref(&share.branch),
                share.base_branch.as_deref(),
            )?;
            fetch(&setup.remote, interaction.human_progress)?;
            let dir = repo
                .work_tree()
                .map(grit_lib::config::git_path::user_display_path)
                .unwrap_or_else(|| grit_lib::config::git_path::user_display_path(repo.git_dir()));
            (Some(dir), false)
        }
        None => {
            let cloned = crate::commands::clone::run(
                crate::commands::clone::CloneRunOptions {
                    url: url.clone(),
                    dir: None,
                    depth: None,
                    branch: Some(share.branch.clone()),
                    shallow: false,
                    single_branch: Some(false),
                    no_tags: false,
                    origin: None,
                    bare: false,
                    no_checkout: false,
                    blobs: None,
                    config: Vec::new(),
                    quiet: crate::quiet_args::QuietArgs { quiet: true },
                },
                interaction.human_progress,
            )
            .with_context(|| format!("cloning {url}"))?;
            // grit runs from where it started, so open the new clone by path.
            let (options, _sink) = crate::diagnostics::repository_options(false);
            let repo = Repository::discover(Some(std::path::Path::new(&cloned.path)), &options)
                .with_context(|| format!("opening {}", cloned.path))?;
            configure_shared(
                &repo,
                "origin",
                std::slice::from_ref(&share.branch),
                share.base_branch.as_deref(),
            )?;
            crate::commands::sync::fetch_in(&repo, "origin", interaction.human_progress)?;
            (Some(cloned.path), true)
        }
    };
    Ok(ShareAcceptOutcome {
        status: status.to_owned(),
        branch: share.branch,
        sharer: share.sharer,
        origin: share.origin,
        project,
        url,
        path,
        cloned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_are_a_number_or_unlimited() {
        assert_eq!(parse_uses("3").unwrap(), LinkUses::Count(3));
        assert_eq!(parse_uses("Unlimited").unwrap(), LinkUses::Unlimited);
        assert!(parse_uses("0").is_err());
        assert!(parse_uses("many").is_err());
    }
}
