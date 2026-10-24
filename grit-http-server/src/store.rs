//! Users, access tokens and repository grants for `grit serve`, kept in SQLite.
//!
//! Every request to the server carries an access token (as the password of
//! HTTP basic auth, or as a bearer token). The store maps the token to a
//! [`User`] and answers what [`Access`] that user has to a repository:
//!
//! - an admin user can read and write every repository;
//! - any other user can use only the repositories they were granted, with the
//!   granted access.
//!
//! Tokens are stored as SHA-256 hashes, so a copy of the database does not
//! reveal them. The plaintext is shown once, when the token is created.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// Errors from the user and token store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Reading or writing the database failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// The operating system could not supply random bytes for a token.
    #[error("cannot generate a random token: {0}")]
    Random(getrandom::Error),
    /// [`AuthStore::setup`] was called on a database that already has users.
    #[error("the server is already set up")]
    AlreadySetUp,
    /// A user with this name already exists.
    #[error("user '{0}' already exists")]
    UserExists(String),
    /// No user has this name.
    #[error("no user named '{0}'")]
    UnknownUser(String),
    /// User names are 1–64 ASCII letters, digits, `.`, `_` or `-`.
    #[error("invalid user name '{0}'")]
    InvalidUserName(String),
    /// Repository names are one path component without a leading `.`.
    #[error("invalid repository name '{0}'")]
    InvalidRepositoryName(String),
}

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// What a user may do with a repository. `Write` includes `Read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Fetch and clone.
    Read,
    /// Fetch, clone and push.
    Write,
}

impl Access {
    /// The name stored in the database and shown to people: `read` or `write`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

/// A server user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// Database row id.
    pub id: i64,
    /// Unique user name.
    pub name: String,
    /// Admins can read and write every repository.
    pub admin: bool,
}

/// One repository grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Repository name as served (without a `.git` suffix).
    pub repository: String,
    /// Granted access.
    pub access: Access,
}

/// A user with their grants and token count, from [`AuthStore::users`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSummary {
    /// The user.
    pub user: User,
    /// Repository grants, sorted by repository name.
    pub grants: Vec<Grant>,
    /// Number of access tokens the user has.
    pub tokens: usize,
}

/// A plaintext access token.
///
/// Only its hash is stored; keep the plaintext to hand to the user.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Prefix of every generated token, so tokens are recognisable in configs and logs.
    pub const PREFIX: &'static str = "grit_";

    /// Generate a new random token (160 bits from the operating system).
    ///
    /// # Errors
    ///
    /// [`StoreError::Random`] when the system random source fails.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; 20];
        getrandom::getrandom(&mut bytes).map_err(StoreError::Random)?;
        Ok(Self(format!("{}{}", Self::PREFIX, hex::encode(bytes))))
    }

    /// Wrap a token presented by a client, or a fixed token in tests.
    #[must_use]
    pub fn from_secret(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The plaintext token.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.0
    }

    fn hash(&self) -> String {
        hex::encode(Sha256::digest(self.0.as_bytes()))
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Strip a trailing `.git` and check that `name` can name a served repository.
///
/// # Errors
///
/// [`StoreError::InvalidRepositoryName`] when the name is empty, contains `/`
/// or `\`, or starts with `.`.
pub fn normalize_repository_name(name: &str) -> Result<&str> {
    let trimmed = name.strip_suffix(".git").unwrap_or(name);
    let valid = !trimmed.is_empty() && !trimmed.starts_with('.') && !trimmed.contains(['/', '\\']);
    if valid {
        Ok(trimmed)
    } else {
        Err(StoreError::InvalidRepositoryName(name.to_owned()))
    }
}

fn validate_user_name(name: &str) -> Result<()> {
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidUserName(name.to_owned()))
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    admin INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS tokens (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS grants (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repository TEXT NOT NULL,
    access TEXT NOT NULL CHECK (access IN ('read', 'write')),
    PRIMARY KEY (user_id, repository)
);
PRAGMA user_version = 1;
";

/// The SQLite-backed user, token and grant store.
pub struct AuthStore {
    conn: Connection,
}

impl AuthStore {
    /// Open the database at `path`, creating it and its tables when missing.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the file cannot be opened or initialised.
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Open a private in-memory database (for tests).
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when SQLite fails to initialise.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Whether any user exists yet.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the query fails.
    pub fn has_users(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// Create the first admin user with `token`, on a database with no users.
    ///
    /// # Errors
    ///
    /// [`StoreError::AlreadySetUp`] when a user already exists, or the errors
    /// of [`Self::add_user`].
    pub fn setup(&mut self, admin_name: &str, token: &Token) -> Result<User> {
        if self.has_users()? {
            return Err(StoreError::AlreadySetUp);
        }
        let user = self.add_user(admin_name, true)?;
        self.add_token(&user, token)?;
        Ok(user)
    }

    /// Create a user.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidUserName`], [`StoreError::UserExists`], or
    /// [`StoreError::Sqlite`].
    pub fn add_user(&mut self, name: &str, admin: bool) -> Result<User> {
        validate_user_name(name)?;
        if self.user(name)?.is_some() {
            return Err(StoreError::UserExists(name.to_owned()));
        }
        self.conn.execute(
            "INSERT INTO users (name, admin) VALUES (?1, ?2)",
            params![name, admin],
        )?;
        Ok(User {
            id: self.conn.last_insert_rowid(),
            name: name.to_owned(),
            admin,
        })
    }

    /// Look up a user by name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the query fails.
    pub fn user(&self, name: &str) -> Result<Option<User>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, admin FROM users WHERE name = ?1",
                params![name],
                |row| {
                    Ok(User {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        admin: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Look up a user by name, failing when there is none.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnknownUser`] or [`StoreError::Sqlite`].
    pub fn require_user(&self, name: &str) -> Result<User> {
        self.user(name)?
            .ok_or_else(|| StoreError::UnknownUser(name.to_owned()))
    }

    /// Add `token` to `user`'s tokens.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the insert fails (including a token that is
    /// already stored).
    pub fn add_token(&mut self, user: &User, token: &Token) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tokens (user_id, token_hash) VALUES (?1, ?2)",
            params![user.id, token.hash()],
        )?;
        Ok(())
    }

    /// The user `token` belongs to, or `None` for an unknown token.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the query fails.
    pub fn authenticate(&self, token: &Token) -> Result<Option<User>> {
        Ok(self
            .conn
            .query_row(
                "SELECT users.id, users.name, users.admin FROM tokens
                 JOIN users ON users.id = tokens.user_id
                 WHERE tokens.token_hash = ?1",
                params![token.hash()],
                |row| {
                    Ok(User {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        admin: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Give `user` `access` to `repository`, replacing any earlier grant.
    ///
    /// `repository` may carry a `.git` suffix; it is stored without one.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidRepositoryName`] or [`StoreError::Sqlite`].
    pub fn grant(&mut self, user: &User, repository: &str, access: Access) -> Result<Grant> {
        let repository = normalize_repository_name(repository)?;
        self.conn.execute(
            "INSERT INTO grants (user_id, repository, access) VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id, repository) DO UPDATE SET access = excluded.access",
            params![user.id, repository, access.as_str()],
        )?;
        Ok(Grant {
            repository: repository.to_owned(),
            access,
        })
    }

    /// The access `user` has to `repository`, or `None` for no access.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the query fails.
    pub fn access(&self, user: &User, repository: &str) -> Result<Option<Access>> {
        if user.admin {
            return Ok(Some(Access::Write));
        }
        let access: Option<String> = self
            .conn
            .query_row(
                "SELECT access FROM grants WHERE user_id = ?1 AND repository = ?2",
                params![user.id, repository],
                |row| row.get(0),
            )
            .optional()?;
        Ok(access.as_deref().and_then(Access::from_db))
    }

    /// Every user with their grants and token count, sorted by name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when a query fails.
    pub fn users(&self) -> Result<Vec<UserSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, admin, (SELECT COUNT(*) FROM tokens WHERE user_id = users.id)
             FROM users ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                User {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    admin: row.get(2)?,
                },
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut grants = self.conn.prepare(
            "SELECT repository, access FROM grants WHERE user_id = ?1 ORDER BY repository",
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (user, tokens) = row?;
            let user_grants = grants
                .query_map(params![user.id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .filter_map(|row| match row {
                    Ok((repository, access)) => {
                        Access::from_db(&access).map(|access| Ok(Grant { repository, access }))
                    }
                    Err(e) => Some(Err(e)),
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.push(UserSummary {
                user,
                grants: user_grants,
                tokens: usize::try_from(tokens).unwrap_or_default(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AuthStore {
        match AuthStore::open_in_memory() {
            Ok(store) => store,
            Err(e) => panic!("open store: {e}"),
        }
    }

    fn ok<T>(result: Result<T>) -> T {
        match result {
            Ok(v) => v,
            Err(e) => panic!("store call failed: {e}"),
        }
    }

    #[test]
    fn setup_creates_one_admin_and_only_once() {
        let mut store = store();
        assert!(!ok(store.has_users()));
        let token = Token::from_secret("grit_admin");
        let admin = ok(store.setup("admin", &token));
        assert!(admin.admin);
        assert_eq!(ok(store.authenticate(&token)), Some(admin));
        assert!(matches!(
            store.setup("other", &Token::from_secret("grit_other")),
            Err(StoreError::AlreadySetUp)
        ));
    }

    #[test]
    fn unknown_tokens_do_not_authenticate() {
        let mut store = store();
        ok(store.setup("admin", &Token::from_secret("grit_admin")));
        assert_eq!(
            ok(store.authenticate(&Token::from_secret("grit_nope"))),
            None
        );
    }

    #[test]
    fn admins_write_everything_and_users_only_their_grants() {
        let mut store = store();
        let admin = ok(store.setup("admin", &Token::from_secret("grit_admin")));
        let bob = ok(store.add_user("bob", false));
        assert_eq!(ok(store.access(&admin, "anything")), Some(Access::Write));
        assert_eq!(ok(store.access(&bob, "project")), None);

        ok(store.grant(&bob, "project.git", Access::Read));
        assert_eq!(ok(store.access(&bob, "project")), Some(Access::Read));
        ok(store.grant(&bob, "project", Access::Write));
        assert_eq!(ok(store.access(&bob, "project")), Some(Access::Write));
        assert_eq!(ok(store.access(&bob, "other")), None);
    }

    #[test]
    fn users_lists_grants_and_token_counts() {
        let mut store = store();
        ok(store.setup("admin", &Token::from_secret("grit_admin")));
        let bob = ok(store.add_user("bob", false));
        ok(store.add_token(&bob, &Token::from_secret("grit_b1")));
        ok(store.add_token(&bob, &Token::from_secret("grit_b2")));
        ok(store.grant(&bob, "zeta", Access::Read));
        ok(store.grant(&bob, "alpha", Access::Write));

        let users = ok(store.users());
        let names: Vec<_> = users.iter().map(|u| u.user.name.as_str()).collect();
        assert_eq!(names, ["admin", "bob"]);
        assert_eq!(users[1].tokens, 2);
        let repos: Vec<_> = users[1]
            .grants
            .iter()
            .map(|g| (g.repository.as_str(), g.access))
            .collect();
        assert_eq!(repos, [("alpha", Access::Write), ("zeta", Access::Read)]);
    }

    #[test]
    fn names_are_validated() {
        let mut store = store();
        assert!(matches!(
            store.add_user("no spaces", false),
            Err(StoreError::InvalidUserName(_))
        ));
        ok(store.add_user("bob", false));
        assert!(matches!(
            store.add_user("bob", true),
            Err(StoreError::UserExists(_))
        ));
        assert!(matches!(
            store.require_user("carol"),
            Err(StoreError::UnknownUser(_))
        ));
        for bad in ["", ".git", "a/b", "..", ".hidden"] {
            assert!(normalize_repository_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(ok(normalize_repository_name("project.git")), "project");
    }

    #[test]
    fn generated_tokens_are_prefixed_and_distinct() {
        let a = ok(Token::generate());
        let b = ok(Token::generate());
        assert!(a.secret().starts_with(Token::PREFIX));
        assert_eq!(a.secret().len(), Token::PREFIX.len() + 40);
        assert_ne!(a, b);
        assert_eq!(format!("{a:?}"), "Token(..)");
    }

    #[test]
    fn database_persists_across_opens() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("tempdir: {e}"),
        };
        let path = dir.path().join("serve.db");
        let token = Token::from_secret("grit_persist");
        {
            let mut store = ok(AuthStore::open(&path));
            ok(store.setup("admin", &token));
        }
        let store = ok(AuthStore::open(&path));
        assert_eq!(
            ok(store.authenticate(&token)).map(|u| u.name),
            Some("admin".to_owned())
        );
    }
}
