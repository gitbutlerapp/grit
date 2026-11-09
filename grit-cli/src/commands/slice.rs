//! `grit slice` — named sets of paths to check out together, kept on the
//! server's target branch.

pub use crate::render::slice::*;
use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::snapshot_checkout::{
    CheckoutClient, CheckoutMarker, Ident, Slice, SliceChange, SliceDeleteRequest, SliceRequest,
    SLICES_PATH,
};
use grit_lib::remote::{redact_url_for_display, HttpClientFactory};

impl From<Slice> for SliceJson {
    fn from(s: Slice) -> Self {
        Self {
            name: s.name,
            description: s.description,
            paths: s.paths,
        }
    }
}

/// The server to talk to: `--url`, else the checkout the current directory is in.
struct Target {
    url: String,
    config: ConfigSet,
    http: Box<dyn grit_lib::remote::http_client::HttpClient>,
}

impl Target {
    fn open(url: Option<&str>) -> Result<Self> {
        let env = crate::context::environment();
        let repo = crate::context::discover().ok();
        let marker = repo
            .as_ref()
            .and_then(|r| CheckoutMarker::read(r.git_dir()));
        let url = match (url, marker) {
            (Some(u), _) => u.to_owned(),
            (None, Some(m)) => m.url,
            (None, None) => {
                bail!("not in a snapshot checkout; pass --url <repository> to name the server")
            }
        };
        let config = ConfigSet::load(
            &env,
            repo.as_ref().map(|r| r.git_dir()),
            &grit_lib::config::LoadConfigOptions::standard(
                &env,
                repo.as_ref().map(|r| r.git_dir()),
            ),
        )
        .context("could not load config")?;
        let http = crate::context::http_client_factory()
            .create(&config)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self { url, config, http })
    }

    fn client(&self) -> Result<CheckoutClient<'_>> {
        Ok(CheckoutClient::new(self.http.as_ref(), &self.url)?)
    }

    fn author(&self) -> Option<Ident> {
        crate::commands::checkout::author_for(&self.config)
    }
}

/// `grit slice list [--url <url>]`.
pub fn list(url: Option<&str>) -> Result<SliceListOutcome> {
    let target = Target::open(url)?;
    let client = target.client()?;
    let slices = client.slices()?;
    Ok(SliceListOutcome {
        url: redact_url_for_display(&client.url().repo_url),
        target: slices.target,
        head: slices.head,
        slices: slices.slices.into_iter().map(SliceJson::from).collect(),
    })
}

fn changed(action: &str, name: &str, change: SliceChange) -> SliceChangeOutcome {
    SliceChangeOutcome {
        action: action.to_owned(),
        name: name.to_owned(),
        slice: change.slice.map(SliceJson::from),
        target: change.target,
        previous: change.previous,
        head: change.head,
    }
}

/// `grit slice new <name> --path <path>... [--description <text>]`.
pub fn new(
    url: Option<&str>,
    name: &str,
    paths: &[String],
    description: Option<&str>,
) -> Result<SliceChangeOutcome> {
    if paths.is_empty() {
        bail!("a slice needs at least one --path");
    }
    let target = Target::open(url)?;
    let change = target.client()?.put_slice(
        name,
        &SliceRequest {
            paths: paths.to_vec(),
            description: description.map(str::to_owned),
            create: true,
            author: target.author(),
        },
    )?;
    Ok(changed("new", name, change))
}

/// `grit slice edit <name> [--add <path>]... [--remove <path>]... [--description <text>]`.
pub fn edit(
    url: Option<&str>,
    name: &str,
    add: &[String],
    remove: &[String],
    description: Option<&str>,
) -> Result<SliceChangeOutcome> {
    let target = Target::open(url)?;
    let client = target.client()?;
    let current = client.slices()?;
    let Some(slice) = current.slices.into_iter().find(|s| s.name == name) else {
        bail!("there is no slice {name} (see `grit slice list`; {SLICES_PATH} holds them)");
    };
    let mut paths: Vec<String> = slice
        .paths
        .into_iter()
        .filter(|p| !remove.iter().any(|r| r.trim_matches('/') == p))
        .collect();
    paths.extend(add.iter().cloned());
    let change = client.put_slice(
        name,
        &SliceRequest {
            paths,
            description: description.map(str::to_owned),
            create: false,
            author: target.author(),
        },
    )?;
    Ok(changed("edit", name, change))
}

/// `grit slice delete <name>`.
pub fn delete(url: Option<&str>, name: &str) -> Result<SliceChangeOutcome> {
    let target = Target::open(url)?;
    let change = target.client()?.delete_slice(
        name,
        &SliceDeleteRequest {
            author: target.author(),
        },
    )?;
    Ok(changed("delete", name, change))
}

/// Dispatch parsed CLI args for `Slice`.
pub fn dispatch(
    args: crate::args::SliceArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let SliceArgs { action, url } = args;
    match action {
        SliceAction::List => emit_success(commands::slice::list(url.as_deref())?, opts, ctx),
        SliceAction::New {
            name,
            paths,
            description,
        } => emit_success(
            commands::slice::new(url.as_deref(), &name, &paths, description.as_deref())?,
            opts,
            ctx,
        ),
        SliceAction::Edit {
            name,
            add,
            remove,
            description,
        } => emit_success(
            commands::slice::edit(url.as_deref(), &name, &add, &remove, description.as_deref())?,
            opts,
            ctx,
        ),
        SliceAction::Delete { name } => {
            emit_success(commands::slice::delete(url.as_deref(), &name)?, opts, ctx)
        }
    }
}
