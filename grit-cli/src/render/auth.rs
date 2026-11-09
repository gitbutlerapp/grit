//! JSON outcome types and rendering for `grit auth`.

use std::io::Write;

use anyhow::Result;
use grit_lib::remote::github_auth::GITHUB_HOST;
use schemars::JsonSchema;
use serde::Serialize;

use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit auth` (sign-in).
#[derive(Serialize, JsonSchema)]
pub struct AuthOutcome {
    pub authenticated: bool,
    pub host: String,
    /// The user the token belongs to on a `grit serve` host, or the `--username`
    /// it was stored for (absent for GitHub).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Whether the host confirmed the token. `false` for `--username`/`--token`,
    /// which stores it without contacting the host.
    pub verified: bool,
}

impl HumanRender for AuthOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match &self.user {
            Some(user) if !self.verified => outln!(
                w,
                "✓ Stored the token for {user} on {} — the next push or fetch uses it.",
                self.host
            ),
            Some(user) => outln!(w, "✓ Signed in to {} as {user} — token stored.", self.host),
            None => outln!(
                w,
                "\n✓ Signed in to GitHub — token stored for {}.",
                self.host
            ),
        }
        Ok(())
    }
}

impl MarkdownRender for AuthOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_auth_outcome(w, self)?;
        Ok(())
    }
}

/// Stored credential fields reported by `grit auth logout --dry-run`.
#[derive(Serialize, JsonSchema)]
pub struct LogoutCredentialPreview {
    pub protocol: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// Result of `grit auth logout`.
#[derive(Serialize, JsonSchema)]
pub struct LogoutOutcome {
    pub logged_out: bool,
    pub host: String,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(skip_serializing_if = "is_false")]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub credential: Option<LogoutCredentialPreview>,
}

impl HumanRender for LogoutOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            if let Some(cred) = &self.credential {
                outln!(
                    w,
                    "Dry run: would remove stored credential for {} ({}) on {}.",
                    cred.host,
                    cred.username.as_deref().unwrap_or("(no username)"),
                    cred.protocol
                );
            } else {
                outln!(
                    w,
                    "Dry run: no stored credential for {} would be removed.",
                    self.host
                );
            }
            return Ok(());
        }
        if self.logged_out {
            let service = if self.host == GITHUB_HOST {
                "GitHub"
            } else {
                &self.host
            };
            outln!(
                w,
                "✓ Signed out of {service} — removed the stored token for {}.",
                self.host
            );
        } else {
            outln!(
                w,
                "No credential helper is configured, so there's no stored token for {} to remove.",
                self.host
            );
        }
        Ok(())
    }
}

impl MarkdownRender for LogoutOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_logout_outcome(w, self)?;
        Ok(())
    }
}
