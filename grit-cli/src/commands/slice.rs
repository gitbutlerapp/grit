//! `grit slice` — named sets of paths to check out together, kept on the
//! server's target branch.

use std::io::Write;

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::snapshot_checkout::{
    CheckoutClient, CheckoutMarker, Ident, Slice, SliceChange, SliceDeleteRequest, SliceRequest,
    SLICES_PATH,
};
use grit_lib::remote::HttpClientFactory;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// A slice as `grit slice` reports it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SliceJson {
    /// The slice's name.
    pub name: String,
    /// What it is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its paths.
    pub paths: Vec<String>,
}

impl From<Slice> for SliceJson {
    fn from(s: Slice) -> Self {
        Self {
            name: s.name,
            description: s.description,
            paths: s.paths,
        }
    }
}

/// Result of `grit slice list`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SliceListOutcome {
    /// The server repository.
    pub url: String,
    /// The target branch the slices are on.
    pub target: String,
    /// Where it is.
    pub head: String,
    /// The slices.
    pub slices: Vec<SliceJson>,
}

impl HumanRender for SliceListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.slices.is_empty() {
            outln!(
                w,
                "{} has no slices yet; make one with `grit slice new <name> --path <dir>`.",
                self.target
            );
            return Ok(());
        }
        let width = self.slices.iter().map(|s| s.name.len()).max().unwrap_or(0);
        for s in &self.slices {
            outln!(w, "{:<width$}  {}", s.name, s.paths.join(", "));
            if let Some(d) = &s.description {
                outln!(w, "{:<width$}  {d}", "");
            }
        }
        Ok(())
    }
}

impl MarkdownRender for SliceListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Slices on `{}`", self.target);
        outln!(w);
        if self.slices.is_empty() {
            outln!(w, "No slices.");
            return Ok(());
        }
        outln!(w, "| Slice | Paths | Description |");
        outln!(w, "| --- | --- | --- |");
        for s in &self.slices {
            let paths: Vec<String> = s.paths.iter().map(|p| format!("`{p}`")).collect();
            outln!(
                w,
                "| `{}` | {} | {} |",
                s.name,
                paths.join(", "),
                s.description.as_deref().unwrap_or_default()
            );
        }
        Ok(())
    }
}

/// Result of `grit slice new`, `edit` and `delete`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SliceChangeOutcome {
    /// `new`, `edit` or `delete`.
    pub action: String,
    /// The slice's name.
    pub name: String,
    /// The slice as it is now; omitted once deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice: Option<SliceJson>,
    /// The target branch.
    pub target: String,
    /// Where it was.
    pub previous: String,
    /// Where it is: the server's commit changing the slices file.
    pub head: String,
}

impl HumanRender for SliceChangeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let verb = match self.action.as_str() {
            "new" => "Added",
            "edit" => "Changed",
            _ => "Removed",
        };
        let short = |s: &str| s.get(..7).unwrap_or(s).to_owned();
        if self.previous == self.head {
            outln!(w, "· slice {} is unchanged", self.name);
        } else {
            outln!(
                w,
                "✓ {verb} slice {} on {}: {} → {}",
                self.name,
                self.target,
                short(&self.previous),
                short(&self.head)
            );
        }
        if let Some(s) = &self.slice {
            outln!(w, "  {}", s.paths.join(", "));
        }
        Ok(())
    }
}

impl MarkdownRender for SliceChangeOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Slice `{}` ({})", self.name, self.action);
        outln!(w);
        outln!(
            w,
            "- **target**: `{}` `{}` → `{}`",
            self.target,
            self.previous,
            self.head
        );
        if let Some(s) = &self.slice {
            let paths: Vec<String> = s.paths.iter().map(|p| format!("`{p}`")).collect();
            outln!(w, "- **paths**: {}", paths.join(", "));
        }
        Ok(())
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
        let config = ConfigSet::load(&env, repo.as_ref().map(|r| r.git_dir()), true)
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
        let email = self.config.get("user.email")?;
        let name = self
            .config
            .get("user.name")
            .unwrap_or_else(|| email.clone());
        Some(Ident { name, email })
    }
}

/// `grit slice list [--url <url>]`.
pub fn list(url: Option<&str>) -> Result<SliceListOutcome> {
    let target = Target::open(url)?;
    let client = target.client()?;
    let slices = client.slices()?;
    Ok(SliceListOutcome {
        url: client.url().repo_url.clone(),
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
