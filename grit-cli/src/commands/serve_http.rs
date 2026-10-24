//! `grit serve` — serve repositories over smart HTTP with token authentication.
//!
//! The server, the user database and the web pages live in
//! [`grit_http_server`]; this module parses paths, opens the database and
//! renders results.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use grit_http_server::server::{default_database_path, ServeConfig, Server, ServerError};
use grit_http_server::store::{Access, AuthStore, Token};
use schemars::JsonSchema;
use serde::Serialize;

use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// The database path for `--db`, defaulting next to (or inside) `root`.
fn database_path(root: &Path, db: Option<&Path>) -> PathBuf {
    db.map_or_else(|| default_database_path(root), Path::to_path_buf)
}

/// Open an existing database; only `--setup` creates one.
fn open_existing(database: &Path) -> Result<AuthStore> {
    if !database.is_file() {
        bail!(
            "no server database at {}; run `grit serve --setup` first",
            database.display()
        );
    }
    AuthStore::open(database).with_context(|| format!("opening {}", database.display()))
}

/// Run the server until interrupted.
///
/// # Parameters
///
/// - `root`: a repository, or a directory of repositories.
/// - `db`: the user database (`--db`), or `None` for the default.
/// - `bind`: `host:port` to listen on.
///
/// # Errors
///
/// Fails when the database has no users, cannot be opened, or the address
/// cannot be bound.
pub fn run(root: &Path, db: Option<&Path>, bind: &str) -> Result<()> {
    let root =
        std::fs::canonicalize(root).with_context(|| format!("cannot serve {}", root.display()))?;
    let database = database_path(&root, db);
    open_existing(&database)?;
    let config = ServeConfig {
        root: root.clone(),
        database,
        repository_options: grit_protocol::repository_options_from_process(),
    };
    let server = match Server::bind(config, bind) {
        Ok(server) => server,
        Err(ServerError::NotSetUp) => bail!("the server has no users; run `grit serve --setup`"),
        Err(e) => return Err(e).with_context(|| format!("starting the server on {bind}")),
    };
    let addr = server.local_addr()?;
    let repositories = server.repositories();
    eprintln!(
        "Serving {} {} from {} at http://{addr}/",
        repositories.len(),
        if repositories.len() == 1 {
            "repository"
        } else {
            "repositories"
        },
        root.display()
    );
    for repo in &repositories {
        eprintln!("  http://{addr}/{}.git", repo.name);
    }
    server.run()?;
    Ok(())
}

/// Result of `grit serve --setup` and `grit serve add-user`: a new user and their token.
#[derive(Serialize, JsonSchema)]
pub struct ServeUserOutcome {
    /// The user database the user was added to.
    pub database: String,
    /// The new user's name.
    pub user: String,
    /// Whether the user is an admin (read and write on every repository).
    pub admin: bool,
    /// The user's access token. It is shown only once.
    pub token: String,
}

/// Create the database and its first admin user.
///
/// # Errors
///
/// Fails when the database already has users or cannot be written.
pub fn setup(root: &Path, db: Option<&Path>, admin: &str) -> Result<ServeUserOutcome> {
    let database = database_path(root, db);
    let mut store =
        AuthStore::open(&database).with_context(|| format!("opening {}", database.display()))?;
    let token = Token::generate()?;
    let user = store.setup(admin, &token)?;
    Ok(ServeUserOutcome {
        database: database.display().to_string(),
        user: user.name,
        admin: user.admin,
        token: token.secret().to_owned(),
    })
}

/// Add a user with a fresh token.
///
/// # Errors
///
/// Fails when there is no database, or the name is invalid or taken.
pub fn add_user(
    root: &Path,
    db: Option<&Path>,
    name: &str,
    admin: bool,
) -> Result<ServeUserOutcome> {
    let database = database_path(root, db);
    let mut store = open_existing(&database)?;
    let token = Token::generate()?;
    let user = store.add_user(name, admin)?;
    store.add_token(&user, &token)?;
    Ok(ServeUserOutcome {
        database: database.display().to_string(),
        user: user.name,
        admin: user.admin,
        token: token.secret().to_owned(),
    })
}

impl HumanRender for ServeUserOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let role = if self.admin { "admin user" } else { "user" };
        outln!(w, "✓ Created {role} {} in {}", self.user, self.database);
        outln!(w);
        outln!(w, "  Token: {}", self.token);
        outln!(w);
        outln!(
            w,
            "Save the token now; it is not shown again. Use it as the password when Git asks."
        );
        Ok(())
    }
}

impl MarkdownRender for ServeUserOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_serve_user_outcome(w, self)
    }
}

/// Result of `grit serve grant`.
#[derive(Serialize, JsonSchema)]
pub struct ServeGrantOutcome {
    /// The user given access.
    pub user: String,
    /// The repository name, without `.git`.
    pub repository: String,
    /// `read` or `write`.
    pub access: String,
}

/// Give a user read (or, with `write`, write) access to a repository.
///
/// # Errors
///
/// Fails when there is no database, the user does not exist, or the
/// repository name is invalid.
pub fn grant(
    root: &Path,
    db: Option<&Path>,
    user: &str,
    repository: &str,
    write: bool,
) -> Result<ServeGrantOutcome> {
    let mut store = open_existing(&database_path(root, db))?;
    let user = store.require_user(user)?;
    let access = if write { Access::Write } else { Access::Read };
    let grant = store.grant(&user, repository, access)?;
    Ok(ServeGrantOutcome {
        user: user.name,
        repository: grant.repository,
        access: grant.access.as_str().to_owned(),
    })
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
    /// User name.
    pub name: String,
    /// Admins read and write every repository.
    pub admin: bool,
    /// How many access tokens the user has.
    pub tokens: usize,
    /// Repository grants, sorted by repository.
    pub grants: Vec<ServeGrantJson>,
}

/// Result of `grit serve users`.
#[derive(Serialize, JsonSchema)]
pub struct ServeUsersOutcome {
    /// The user database read.
    pub database: String,
    /// Every user, sorted by name.
    pub users: Vec<ServeUserJson>,
}

/// List users, their token counts and grants.
///
/// # Errors
///
/// Fails when there is no database or it cannot be read.
pub fn users(root: &Path, db: Option<&Path>) -> Result<ServeUsersOutcome> {
    let database = database_path(root, db);
    let store = open_existing(&database)?;
    let users = store
        .users()?
        .into_iter()
        .map(|summary| ServeUserJson {
            name: summary.user.name,
            admin: summary.user.admin,
            tokens: summary.tokens,
            grants: summary
                .grants
                .into_iter()
                .map(|g| ServeGrantJson {
                    repository: g.repository,
                    access: g.access.as_str().to_owned(),
                })
                .collect(),
        })
        .collect();
    Ok(ServeUsersOutcome {
        database: database.display().to_string(),
        users,
    })
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

impl HumanRender for ServeUsersOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let width = self.users.iter().map(|u| u.name.len()).max().unwrap_or(0);
        for user in &self.users {
            let unit = if user.tokens == 1 { "token" } else { "tokens" };
            let tokens = format!("{} {unit}", user.tokens);
            outln!(
                w,
                "{:width$}  {tokens:<9}  {}",
                user.name,
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
