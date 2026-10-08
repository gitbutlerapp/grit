//! `grit push` — publish the current branch to its remote.
//!
//! No upstream ceremony: `grit push` sends the current branch to `origin` (or the
//! configured `branch.<name>.remote`) under the same name, creating it on the
//! remote if needed.

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::push_report::PushRefStatus;
use grit_lib::refs;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::transfer::PushRefSpec;
use serde::Serialize;

use crate::commands::auth;
use crate::context;
use crate::net;
use crate::output::HumanRender;

/// Result of `grit push`: the per-ref outcomes for the remote.
#[derive(Serialize)]
pub struct PushOutcome {
    pub remote: String,
    /// Local branch (short name) that was pushed, or `"--tags"` when this
    /// push targeted tags instead of the current branch.
    pub branch: String,
    pub results: Vec<PushRefResult>,
    /// True when any ref was rejected (dispatch exits non-zero on this).
    pub rejected: bool,
}

/// One ref's push result.
#[derive(Serialize)]
pub struct PushRefResult {
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// `ok` | `up_to_date` | `rejected`.
    pub status: String,
    /// Rejection detail, present only when `status == "rejected"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HumanRender for PushOutcome {
    fn render_human(&self) {
        for result in &self.results {
            let target = format!("{} {}", self.remote, result.ref_name);
            match result.status.as_str() {
                "ok" => println!("  pushed {} → {target}", self.branch),
                "up_to_date" => println!("  {target} already up to date"),
                // Rejections are diagnostics → stderr (as before).
                _ => eprintln!(
                    "  rejected {target}: {}",
                    result.reason.as_deref().unwrap_or("rejected")
                ),
            }
        }
    }
}

pub fn run(tags: bool) -> Result<PushOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .context("could not load config")?;

    let (branch_label, remote, specs) = if tags {
        let entries =
            refs::list_refs(&repo.git_dir, "refs/tags/").context("could not list tags")?;
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
        ("--tags".to_owned(), net::DEFAULT_REMOTE.to_owned(), specs)
    } else {
        let (short_name, oid) = match resolve_head(&repo.git_dir)? {
            HeadState::Branch {
                short_name,
                oid: Some(oid),
                ..
            } => (short_name, oid),
            HeadState::Branch { .. } => bail!("no commits yet to push"),
            HeadState::Detached { .. } => bail!("HEAD is detached; grit push needs a branch"),
            HeadState::Invalid => bail!("HEAD is in an unknown state"),
        };

        let remote = config
            .get(&format!("branch.{short_name}.remote"))
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| net::DEFAULT_REMOTE.to_owned());
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
        (short_name, remote, vec![spec])
    };

    // On an HTTPS auth failure, `grit auth` can refresh the token and we retry once.
    let report = match net::push(&repo, &config, &remote, &specs) {
        Ok(report) => report,
        Err(err) => {
            let url = net::remote_url(&config, &remote).unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                net::push(&repo, &config, &remote, &specs)?
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
            let (status, reason) = match result.status {
                PushRefStatus::Ok => ("ok", None),
                PushRefStatus::UpToDate => ("up_to_date", None),
                PushRefStatus::RejectNonFastForward => {
                    rejected = true;
                    (
                        "rejected",
                        Some("not a fast-forward — run `grit pull` first".to_owned()),
                    )
                }
                _ => {
                    rejected = true;
                    (
                        "rejected",
                        Some(
                            result
                                .message
                                .clone()
                                .unwrap_or_else(|| "rejected".to_owned()),
                        ),
                    )
                }
            };
            PushRefResult {
                ref_name: result.remote_ref.clone(),
                status: status.to_owned(),
                reason,
            }
        })
        .collect();

    Ok(PushOutcome {
        remote,
        branch: branch_label,
        results,
        rejected,
    })
}
