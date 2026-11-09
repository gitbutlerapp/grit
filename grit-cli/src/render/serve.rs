//! JSON outcome types and rendering for `grit serve users`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// One repository grant in `grit serve users`.
#[derive(Serialize, JsonSchema)]
pub struct ServeGrantJson {
    /// The repository name, without `.git`.
    pub repository: String,
    /// `read` or `write`.
    pub access: String,
}

/// One user in `grit serve users`.
#[derive(Serialize, JsonSchema)]
pub struct ServeUserJson {
    /// The user's email address.
    pub email: String,
    /// Admins read and write every repository.
    pub admin: bool,
    /// The user still has a generated password and must choose their own at
    /// the next sign-in.
    pub temporary_password: bool,
    /// How many access tokens the user has.
    pub tokens: usize,
    /// Repository grants, sorted by repository.
    pub grants: Vec<ServeGrantJson>,
}

impl ServeUserJson {
    /// `all repositories (admin)` or `alpha (write), beta (read)`.
    pub fn access_summary(&self) -> String {
        if self.admin {
            return "all repositories (admin)".to_owned();
        }
        if self.grants.is_empty() {
            return "no repositories".to_owned();
        }
        self.grants
            .iter()
            .map(|g| format!("{} ({})", g.repository, g.access))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Result of `grit serve users`.
#[derive(Serialize, JsonSchema)]
pub struct ServeUsersOutcome {
    /// The user database read.
    pub database: String,
    /// Every user, sorted by name.
    pub users: Vec<ServeUserJson>,
}

impl HumanRender for ServeUsersOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let width = self.users.iter().map(|u| u.email.len()).max().unwrap_or(0);
        for user in &self.users {
            let unit = if user.tokens == 1 { "token" } else { "tokens" };
            let tokens = format!("{} {unit}", user.tokens);
            let temporary = if user.temporary_password {
                "  (temporary password)"
            } else {
                ""
            };
            outln!(
                w,
                "{:width$}  {tokens:<9}  {}{temporary}",
                user.email,
                user.access_summary()
            );
        }
        Ok(())
    }
}

impl MarkdownRender for ServeUsersOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_serve_users_outcome(w, self)
    }
}
