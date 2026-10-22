//! `grit push` — publish the current branch to its remote.
//!
//! No upstream ceremony: `grit push` sends the current branch to `origin` (or the
//! configured `branch.<name>.remote`) under the same name, creating it on the
//! remote if needed.

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::refs;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::PushRefSpec;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::auth;
use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{resolve_push_delete_ref, PushOptions};
use grit_lib::remote::{DefaultHttpClientFactory, ListRefsOptions, Remote, DEFAULT_REMOTE};
use std::collections::HashSet;

/// Result of `grit push`: the per-ref outcomes for the remote.
#[derive(Serialize, JsonSchema)]
pub struct PushOutcome {
    pub remote: String,
    /// Local branch (short name) that was pushed, when there is a single
    /// unambiguous branch. Omitted for `--tags`, multi-ref `--delete`, and
    /// similar pushes that do not map to one branch name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub branch: Option<String>,
    pub results: Vec<PushRefResult>,
    /// True when any ref was rejected (dispatch exits non-zero on this).
    pub rejected: bool,
    /// Human/markdown label for what was pushed (branch name, `--tags`, or `--delete`).
    #[serde(skip)]
    display_label: String,
}

/// Per-ref push status in JSON (`ok`, `up_to_date`, or `rejected`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PushRefOutcomeStatus {
    Ok,
    UpToDate,
    Rejected,
}

impl From<&PushRefStatus> for PushRefOutcomeStatus {
    fn from(status: &PushRefStatus) -> Self {
        match status {
            PushRefStatus::Ok => Self::Ok,
            PushRefStatus::UpToDate => Self::UpToDate,
            _ => Self::Rejected,
        }
    }
}

/// One ref's push result.
#[derive(Serialize, JsonSchema)]
pub struct PushRefResult {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub status: PushRefOutcomeStatus,
    /// Rejection detail, present only when `status == "rejected"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HumanRender for PushOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for result in &self.results {
            let target = format!("{} {}", self.remote, result.ref_name);
            match result.status {
                PushRefOutcomeStatus::Ok => {
                    outln!(w, "  pushed {} → {target}", self.display_label)
                }
                PushRefOutcomeStatus::UpToDate => outln!(w, "  {target} already up to date"),
                PushRefOutcomeStatus::Rejected => outln!(
                    w,
                    "  rejected {target}: {}",
                    result.reason.as_deref().unwrap_or("rejected")
                ),
            }
        }
        Ok(())
    }
}

impl MarkdownRender for PushOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_push_results(w, &self.remote, &self.display_label, &self.results)?;
        Ok(())
    }
}

pub fn run(
    tags: bool,
    delete_refs: &[String],
    remote_override: Option<String>,
) -> Result<PushOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;

    let (branch, display_label, remote, specs) = if !delete_refs.is_empty() {
        let remote = configured_remote_for_push(&repo, &config, remote_override);
        let remote_handle =
            Remote::from_config(&config, &remote).map_err(|e| anyhow::Error::msg(e.to_string()))?;
        let factory = DefaultHttpClientFactory;
        let advertised = remote_handle
            .list_refs(Some(&repo), &ListRefsOptions::default(), Some(&factory))
            .map_err(|e| anyhow::Error::msg(e.to_string()))?;
        let remote_names: HashSet<String> = advertised
            .into_iter()
            .map(|r| r.name)
            .filter(|n| !n.ends_with("^{}"))
            .collect();
        let specs = delete_refs
            .iter()
            .map(|name| map_delete_ref(repo.git_dir(), name, &remote_names))
            .collect::<Result<Vec<_>>>()?;
        let branch = if delete_refs.len() == 1 {
            Some(delete_refs[0].clone())
        } else {
            None
        };
        (branch, "--delete".to_owned(), remote, specs)
    } else if tags {
        let entries =
            refs::list_refs(repo.git_dir(), "refs/tags/").context("could not list tags")?;
        if entries.is_empty() {
            bail!("no tags to push");
        }
        let specs = entries
            .into_iter()
            .map(|(refname, oid)| PushRefSpec {
                src: Some(oid),
                dst: refname,
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
            })
            .collect::<Vec<_>>();
        (
            None,
            "--tags".to_owned(),
            configured_remote_for_push(&repo, &config, remote_override),
            specs,
        )
    } else {
        let (short_name, oid) = match resolve_head(repo.git_dir())? {
            HeadState::Branch {
                short_name,
                oid: Some(oid),
                ..
            } => (short_name, oid),
            HeadState::Branch { .. } => bail!("no commits yet to push"),
            HeadState::Detached { .. } => bail!("HEAD is detached; grit push needs a branch"),
            HeadState::Invalid => bail!("HEAD is in an unknown state"),
        };

        let remote = remote_override
            .or_else(|| {
                config
                    .get(&format!("branch.{short_name}.remote"))
                    .filter(|r| !r.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
        let dst = config
            .get(&format!("branch.{short_name}.merge"))
            .filter(|m| m.starts_with("refs/"))
            .unwrap_or_else(|| format!("refs/heads/{short_name}"));

        let spec = PushRefSpec {
            src: Some(oid),
            dst,
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        };
        (Some(short_name.clone()), short_name, remote, vec![spec])
    };

    // On an HTTPS auth failure, `grit auth` can refresh the token and we retry once.
    let factory = DefaultHttpClientFactory;
    let report = match push_once(&repo, &config, &remote, &specs, &factory) {
        Ok(report) => report,
        Err(err) => {
            let url = config
                .get(&format!("remote.{remote}.url"))
                .unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                push_once(&repo, &config, &remote, &specs, &factory)?
            } else {
                return Err(err);
            }
        }
    };

    let mut rejected = false;
    let results = report
        .results
        .iter()
        .map(|result| {
            let status = PushRefOutcomeStatus::from(&result.status);
            let reason = match result.status {
                PushRefStatus::Ok | PushRefStatus::UpToDate => None,
                PushRefStatus::RejectNonFastForward => {
                    rejected = true;
                    Some("not a fast-forward — run `grit pull` first".to_owned())
                }
                _ => {
                    rejected = true;
                    Some(
                        result
                            .message
                            .clone()
                            .unwrap_or_else(|| "rejected".to_owned()),
                    )
                }
            };
            PushRefResult {
                ref_name: result.remote_ref.clone(),
                status,
                reason,
            }
        })
        .collect();

    Ok(PushOutcome {
        remote,
        branch,
        results,
        rejected,
        display_label,
    })
}

fn configured_remote_for_push(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_override: Option<String>,
) -> String {
    if let Some(remote) = remote_override {
        return remote;
    }
    if let Ok(HeadState::Branch { short_name, .. }) = resolve_head(repo.git_dir()) {
        if let Some(remote) = config
            .get(&format!("branch.{short_name}.remote"))
            .filter(|r| !r.trim().is_empty())
        {
            return remote;
        }
    }
    DEFAULT_REMOTE.to_owned()
}

fn map_delete_ref(
    git_dir: &std::path::Path,
    name: &str,
    remote_ref_names: &HashSet<String>,
) -> Result<PushRefSpec> {
    let dst = resolve_push_delete_ref(git_dir, remote_ref_names, name);
    Ok(PushRefSpec {
        src: None,
        dst,
        force: false,
        delete: true,
        expected_old: None,
        expect_absent: false,
    })
}

fn push_once(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_name: &str,
    specs: &[PushRefSpec],
    factory: &DefaultHttpClientFactory,
) -> Result<grit_lib::remote::transfer::PushOutcome> {
    Remote::from_config(config, remote_name)
        .map_err(|e| anyhow::Error::msg(e.to_string()))?
        .push(
            repo,
            specs,
            PushOptions {
                tracking_remote: Some(remote_name.to_owned()),
                ..Default::default()
            },
            &mut NoProgress,
            Some(factory),
        )
        .map_err(|e| anyhow::Error::msg(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_status_serializes_snake_case() {
        for (status, expected) in [
            (PushRefOutcomeStatus::Ok, "ok"),
            (PushRefOutcomeStatus::UpToDate, "up_to_date"),
            (PushRefOutcomeStatus::Rejected, "rejected"),
        ] {
            let result = PushRefResult {
                ref_name: "refs/heads/main".to_owned(),
                status,
                reason: None,
            };
            let value = serde_json::to_value(&result).expect("json");
            assert_eq!(value["status"].as_str(), Some(expected));
        }
    }
}
