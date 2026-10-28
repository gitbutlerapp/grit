//! `grit serve` — serve repositories over smart HTTP, with a web view where
//! people sign in with an email address and password.
//!
//! The server, the user database and the web pages live in
//! [`grit_http_server`]; this module parses paths, reads passwords from the
//! terminal, opens the database and renders results.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use grit_http_server::server::{default_database_path, ServeConfig, Server, ServerError};
use grit_http_server::store::{generate_password, Access, AuthStore, HashedPassword, Token};
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
pub fn run(
    root: &Path,
    db: Option<&Path>,
    bind: &str,
    bundles: grit_http_server::bundles::BundleSettings,
) -> Result<()> {
    let root =
        std::fs::canonicalize(root).with_context(|| format!("cannot serve {}", root.display()))?;
    let database = database_path(&root, db);
    open_existing(&database)?;
    let config = ServeConfig {
        root: root.clone(),
        database,
        // Used only for upload-pack requests that may lazily fetch promised objects
        // (`uploadpack.lazyFetch`, `GIT_NO_LAZY_FETCH=0`).
        repository_options: grit_protocol::repository_options_from_process().with_promisor_fetcher(
            Some(std::sync::Arc::new(
                grit_lib::objects::promisor::DefaultPromisorFetcher::new(std::sync::Arc::new(
                    crate::context::http_client_factory(),
                )),
            )),
        ),
        bundles,
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
    eprintln!("Sign in at http://{addr}/ (command line: grit auth http://{addr})");
    if bundles.enabled {
        eprintln!(
            "Clone bundles are built in the background and rebuilt every {} after pushes",
            format_interval(bundles.interval)
        );
    }
    server.run()?;
    Ok(())
}

/// Parse a `--bundle-interval` value: a number with `s`, `m`, `h` or `d`
/// (seconds when there is no unit).
///
/// # Errors
///
/// A message naming the bad value.
pub fn parse_interval(value: &str) -> std::result::Result<std::time::Duration, String> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("`{value}` is not a duration like 24h, 30m or 2d"))?;
    let seconds = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(format!("`{value}` is not a duration like 24h, 30m or 2d")),
    };
    Ok(std::time::Duration::from_secs(
        number.saturating_mul(seconds),
    ))
}

/// `86400s` as `24h`, for the startup banner.
fn format_interval(interval: std::time::Duration) -> String {
    let secs = interval.as_secs();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if secs >= size && secs.is_multiple_of(size) {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

/// Result of `grit serve --setup` and `grit serve add-user`: a new user.
#[derive(Serialize, JsonSchema)]
pub struct ServeUserOutcome {
    /// The user database the user was added to.
    pub database: String,
    /// The new user's email address.
    pub user: String,
    /// Whether the user is an admin (read and write on every repository).
    pub admin: bool,
    /// The temporary password generated for the user, shown only once.
    /// Absent when the password was typed in.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Whether the user must choose a new password when they first sign in
    /// (true for a generated password).
    pub temporary: bool,
}

/// A password for a new user: typed in, or `None` to generate a temporary one.
///
/// On a terminal this prompts twice without echoing, and an empty answer
/// means "generate one". Otherwise it reads one line from stdin; no input
/// means "generate one" too.
///
/// # Errors
///
/// Fails when the input cannot be read, or the two answers differ.
pub fn read_new_password(email: &str) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the password from stdin")?;
        let line = line.trim_end_matches(['\r', '\n']);
        return Ok((!line.is_empty()).then(|| line.to_owned()));
    }
    let password = rpassword::prompt_password(format!(
        "Password for {email} (leave empty to generate a temporary one): "
    ))
    .context("reading the password")?;
    if password.is_empty() {
        return Ok(None);
    }
    let again = rpassword::prompt_password("Repeat it: ").context("reading the password")?;
    if again != password {
        bail!("the passwords differ");
    }
    Ok(Some(password))
}

/// The admin email for `--setup`: `--email`, or a prompt on a terminal.
///
/// # Errors
///
/// Fails without `--email` when there is no terminal to ask on.
pub fn read_admin_email(email: Option<String>) -> Result<String> {
    if let Some(email) = email {
        return Ok(email);
    }
    if !std::io::stdin().is_terminal() {
        bail!("pass the admin's email address with --email");
    }
    eprint!("Admin email: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the email address")?;
    Ok(line.trim().to_owned())
}

/// Hash `password`, or generate a temporary one when it is `None`.
/// Returns the hash, the generated plaintext (to show once), and whether it is
/// temporary.
fn new_password(password: Option<String>) -> Result<(HashedPassword, Option<String>, bool)> {
    match password {
        Some(password) => Ok((HashedPassword::new(&password)?, None, false)),
        None => {
            let generated = generate_password()?;
            Ok((HashedPassword::new(&generated)?, Some(generated), true))
        }
    }
}

/// Create the database and its first admin user.
///
/// # Parameters
///
/// - `email`: the admin's email address.
/// - `password`: their password, or `None` to generate a temporary one.
///
/// # Errors
///
/// Fails when the database already has users or cannot be written, or the
/// email or password is not acceptable.
pub fn setup(
    root: &Path,
    db: Option<&Path>,
    email: &str,
    password: Option<String>,
) -> Result<ServeUserOutcome> {
    let database = database_path(root, db);
    let mut store =
        AuthStore::open(&database).with_context(|| format!("opening {}", database.display()))?;
    let (hash, generated, temporary) = new_password(password)?;
    let user = store.setup(email, &hash, temporary)?;
    Ok(ServeUserOutcome {
        database: database.display().to_string(),
        user: user.email,
        admin: user.admin,
        password: generated,
        temporary,
    })
}

/// Add a user.
///
/// # Parameters
///
/// - `email`: the new user's email address.
/// - `password`: their password, or `None` to generate a temporary one.
///
/// # Errors
///
/// Fails when there is no database, the email is invalid or taken, or the
/// password is too short.
pub fn add_user(
    root: &Path,
    db: Option<&Path>,
    email: &str,
    admin: bool,
    password: Option<String>,
) -> Result<ServeUserOutcome> {
    let database = database_path(root, db);
    let mut store = open_existing(&database)?;
    let (hash, generated, temporary) = new_password(password)?;
    let user = store.add_user(email, admin, &hash, temporary)?;
    Ok(ServeUserOutcome {
        database: database.display().to_string(),
        user: user.email,
        admin: user.admin,
        password: generated,
        temporary,
    })
}

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

/// Result of `grit serve token`: a new access token for a user.
#[derive(Serialize, JsonSchema)]
pub struct ServeTokenOutcome {
    /// The user's email address.
    pub user: String,
    /// The new access token. It is shown only once.
    pub token: String,
}

/// Create an access token for a user, for scripts and CI that cannot sign in
/// through the browser.
///
/// # Errors
///
/// Fails when there is no database or no such user.
pub fn token(root: &Path, db: Option<&Path>, email: &str) -> Result<ServeTokenOutcome> {
    let mut store = open_existing(&database_path(root, db))?;
    let user = store.require_user(email)?;
    let token = Token::generate()?;
    store.add_token(&user, &token)?;
    Ok(ServeTokenOutcome {
        user: user.email,
        token: token.secret().to_owned(),
    })
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
        user: user.email,
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
            email: summary.user.email,
            admin: summary.user.admin,
            temporary_password: summary.user.must_change_password,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn intervals_parse_with_units() {
        assert_eq!(parse_interval("24h"), Ok(Duration::from_secs(86_400)));
        assert_eq!(parse_interval("30m"), Ok(Duration::from_secs(1_800)));
        assert_eq!(parse_interval("2d"), Ok(Duration::from_secs(172_800)));
        assert_eq!(parse_interval("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_interval("0s"), Ok(Duration::ZERO));
        assert!(parse_interval("soon").is_err());
        assert!(parse_interval("5w").is_err());
        assert_eq!(format_interval(Duration::from_secs(86_400)), "1d");
        assert_eq!(format_interval(Duration::from_secs(7_200)), "2h");
        assert_eq!(format_interval(Duration::from_secs(90)), "90s");
    }
}
