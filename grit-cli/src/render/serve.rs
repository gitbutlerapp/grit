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

use crate::commands::serve_http::{ServeGrantOutcome, ServeTokenOutcome, ServeUserOutcome};

impl HumanRender for ServeUserOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let role = if self.admin { "admin user" } else { "user" };
        outln!(w, "✓ Created {role} {} in {}", self.user, self.database);
        if let Some(password) = &self.password {
            outln!(w);
            outln!(w, "  Email:    {}", self.user);
            outln!(w, "  Password: {password}");
            outln!(w);
            outln!(
                w,
                "The password is temporary and not shown again: it must be changed at the first sign-in."
            );
        }
        Ok(())
    }
}
impl MarkdownRender for ServeUserOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_serve_user_outcome(w, self)
    }
}
impl HumanRender for ServeTokenOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "✓ Created a token for {}", self.user);
        outln!(w);
        outln!(w, "  Token: {}", self.token);
        outln!(w);
        outln!(
            w,
            "Save it now; it is not shown again. Use it as the password when Git asks, or run `grit auth <url> --token <token>`."
        );
        Ok(())
    }
}
impl MarkdownRender for ServeTokenOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_serve_token_outcome(w, self)
    }
}
impl HumanRender for ServeGrantOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(
            w,
            "✓ {} now has {} access to {}",
            self.user,
            self.access,
            self.repository
        );
        Ok(())
    }
}
impl MarkdownRender for ServeGrantOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_serve_grant_outcome(w, self)
    }
}
