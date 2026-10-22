//! `grit fetch` — download refs and objects from a remote (default `origin`).

use crate::output::OutputMode;
use crate::progress::TerminalProgress;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::transfer::{resolve_fetch_prune, FetchOptions, TagMode};
use grit_lib::remote::{DefaultHttpClientFactory, RemoteError, DEFAULT_REMOTE};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::auth;
use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit fetch`: the refs that changed.
#[derive(Serialize, JsonSchema)]
pub struct FetchOutcome {
    pub remote: String,
    pub updates: Vec<FetchUpdate>,
    pub updated: usize,
}

/// One updated tracking ref. `old_oid`/`new_oid` are full hex, or `null` for a
/// newly-created (`old_oid`) or deleted (`new_oid`) ref.
#[derive(Serialize, JsonSchema)]
pub struct FetchUpdate {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
}

impl HumanRender for FetchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for update in &self.updates {
            let from = update.old_oid.as_deref().map_or("new", short_hex);
            let to = update.new_oid.as_deref().map_or("deleted", short_hex);
            outln!(w, "  {}  {from} → {to}", update.ref_name);
        }
        if self.updated == 0 {
            outln!(w, "Already up to date with {}.", self.remote);
        } else {
            outln!(
                w,
                "Fetched {} update{} from {}.",
                self.updated,
                plural(self.updated),
                self.remote
            );
        }
        Ok(())
    }
}

impl MarkdownRender for FetchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_fetch_updates(w, &self.remote, &self.updates, self.updated)?;
        Ok(())
    }
}

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run(
    remote: Option<String>,
    prune: bool,
    no_prune: bool,
    mode: OutputMode,
) -> Result<FetchOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let remote_name = remote.unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
    let cli_prune = if prune {
        Some(true)
    } else if no_prune {
        Some(false)
    } else {
        None
    };
    let prune_enabled = resolve_fetch_prune(&config, &remote_name, cli_prune)
        .context("could not resolve fetch prune settings")?;

    let factory = DefaultHttpClientFactory;
    let result = match fetch_once(&repo, &config, &remote_name, prune_enabled, &factory, mode) {
        Ok(result) => result,
        Err(err) => {
            let url = config
                .get(&format!("remote.{remote_name}.url"))
                .unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                fetch_once(&repo, &config, &remote_name, prune_enabled, &factory, mode)?
            } else {
                return Err(err);
            }
        }
    };

    let updates: Vec<FetchUpdate> = result
        .updates
        .iter()
        .filter(|update| update.mode.modified_repo())
        .filter_map(|update| {
            let ref_name = update.local_ref.clone()?;
            Some(FetchUpdate {
                ref_name,
                old_oid: update.old_oid.as_ref().map(|o| o.to_hex()),
                new_oid: update.new_oid.as_ref().map(|o| o.to_hex()),
            })
        })
        .collect();
    let updated = updates.len();

    Ok(FetchOutcome {
        remote: remote_name,
        updates,
        updated,
    })
}

fn fetch_once(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_name: &str,
    prune: bool,
    factory: &DefaultHttpClientFactory,
    mode: OutputMode,
) -> Result<grit_lib::remote::transfer::FetchOutcome> {
    let remote = crate::commands::remote::resolve_remote_or_url(Some(config), remote_name)?;
    let mut progress = TerminalProgress::new(mode);
    remote
        .fetch(
            repo,
            FetchOptions {
                tags: TagMode::Following,
                thread_policy: repo.thread_policy(),
                environment: crate::context::environment(),
                prune,
                ..Default::default()
            },
            &mut progress,
            Some(factory),
        )
        .map_err(map_remote_fetch_error)
}

/// Preserve typed [`grit_lib::error::Error::HashCollision`] for JSON output; other library
/// errors keep the pre-mapper string form (no top-level [`grit_lib::error::Error`] downcast).
pub(crate) fn map_remote_error(err: RemoteError) -> anyhow::Error {
    match err {
        RemoteError::Library(grit_lib::error::Error::BundleMissingPrerequisites) => {
            anyhow::Error::new(crate::commands::bundle::MissingPrerequisites)
        }
        RemoteError::Library(grit_lib::error::Error::HashCollision { oid }) => {
            crate::json_error::map_lib_error(grit_lib::error::Error::HashCollision { oid })
        }
        RemoteError::Library(lib) => anyhow::Error::msg(RemoteError::Library(lib).to_string()),
        other => anyhow::Error::msg(other.to_string()),
    }
}

fn map_remote_fetch_error(err: RemoteError) -> anyhow::Error {
    map_remote_error(err)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod map_remote_error_tests {
    use grit_lib::error::Error as LibError;
    use grit_lib::objects::ObjectId;
    use grit_lib::remote::RemoteError;

    use super::map_remote_error;
    use crate::cli_messages::human_error_message;
    use crate::json_error::structured_json_payload;

    #[test]
    fn library_object_not_found_keeps_remote_display() {
        let err = map_remote_error(RemoteError::Library(LibError::ObjectNotFound(
            "deadbeef".into(),
        )));
        assert!(err.downcast_ref::<LibError>().is_none());
        assert_eq!(
            human_error_message(&err),
            "object not found: deadbeef",
            "must not remap to revision-not-found wording"
        );
        assert!(structured_json_payload(&err).is_none());
    }

    #[test]
    fn library_hash_collision_stays_typed_for_json() {
        let oid = ObjectId::from_hex("f92d74e3874587aaf443d1db961d4e26dde13e9c").expect("oid");
        let err = map_remote_error(RemoteError::Library(LibError::HashCollision { oid }));
        let payload = structured_json_payload(&err).expect("structured payload");
        assert_eq!(payload.kind, "hash_collision");
        assert_eq!(payload.oid.as_deref(), Some(oid.to_hex().as_str()));
    }
}
