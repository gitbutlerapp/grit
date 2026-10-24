//! Users, passwords, sessions, access tokens and repository grants for
//! `grit serve`, kept in SQLite.
//!
//! A user is an email address with a password. The password signs in to the
//! web pages, which hands the browser a session; Git requests instead carry an
//! access token (as the password of HTTP basic auth, or as a bearer token),
//! which a user gets by approving a `grit auth <url>` sign-in in the browser.
//! The store maps sessions and tokens to a [`User`] and answers what
//! [`Access`] that user has to a repository:
//!
//! - an admin user can read and write every repository;
//! - any other user can use only the repositories they were granted, with the
//!   granted access.
//!
//! Passwords are stored as Argon2 hashes ([`HashedPassword`]), and tokens and
//! session secrets as SHA-256 hashes, so a copy of the database reveals none of
//! them. A password an admin generates for someone is *temporary*: the user
//! must choose their own the first time they sign in.

use std::path::Path;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// Errors from the user and token store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Reading or writing the database failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// The operating system could not supply random bytes.
    #[error("cannot generate random bytes: {0}")]
    Random(getrandom::Error),
    /// Hashing a password failed.
    #[error("cannot hash the password: {0}")]
    PasswordHash(String),
    /// [`AuthStore::setup`] was called on a database that already has users.
    #[error("the server is already set up")]
    AlreadySetUp,
    /// A user with this email address already exists.
    #[error("user '{0}' already exists")]
    UserExists(String),
    /// No user has this email address.
    #[error("no user '{0}'")]
    UnknownUser(String),
    /// Users are named by an email address, like `alice@example.com`.
    #[error("'{0}' is not an email address")]
    InvalidEmail(String),
    /// Passwords need at least [`MIN_PASSWORD_LENGTH`] characters.
    #[error("passwords need at least {MIN_PASSWORD_LENGTH} characters")]
    WeakPassword,
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
    /// Unique email address, lowercase. It is the user's name on the server.
    pub email: String,
    /// Admins can read and write every repository.
    pub admin: bool,
    /// The password was generated for the user, who must replace it the next
    /// time they sign in.
    pub must_change_password: bool,
}

/// Shortest password [`HashedPassword::new`] accepts.
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// Characters of [`generate_password`]: lowercase letters and digits, without
/// the easily confused `0`, `1`, `i`, `l` and `o`.
const PASSWORD_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// Generate a temporary password, such as `k7mq-3xzp-9wfd-h2ts`.
///
/// Sixteen characters from a 31-letter alphabet (about 79 bits), in groups of
/// four so it can be read out or typed.
///
/// # Errors
///
/// [`StoreError::Random`] when the system random source fails.
pub fn generate_password() -> Result<String> {
    let mut chars = Vec::with_capacity(16);
    let alphabet = PASSWORD_ALPHABET.len();
    // Reject bytes past the last whole multiple of the alphabet so every
    // character is equally likely.
    let limit = 256 - 256 % alphabet;
    while chars.len() < 16 {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(StoreError::Random)?;
        chars.extend(
            bytes
                .iter()
                .map(|&b| usize::from(b))
                .filter(|&b| b < limit)
                .map(|b| char::from(PASSWORD_ALPHABET[b % alphabet])),
        );
    }
    chars.truncate(16);
    Ok(chars
        .chunks(4)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-"))
}

/// An Argon2 password hash, in PHC string form.
///
/// Hashing and verifying take tens of milliseconds on purpose; do it off the
/// async runtime and outside the store lock.
#[derive(Clone, PartialEq, Eq)]
pub struct HashedPassword(String);

impl HashedPassword {
    /// Hash `password` with a fresh random salt.
    ///
    /// # Errors
    ///
    /// [`StoreError::WeakPassword`] when it is shorter than
    /// [`MIN_PASSWORD_LENGTH`] characters, [`StoreError::Random`] when no salt
    /// can be generated, and [`StoreError::PasswordHash`] when hashing fails.
    pub fn new(password: &str) -> Result<Self> {
        if password.chars().count() < MIN_PASSWORD_LENGTH {
            return Err(StoreError::WeakPassword);
        }
        let mut salt = [0u8; 16];
        getrandom::getrandom(&mut salt).map_err(StoreError::Random)?;
        let salt =
            SaltString::encode_b64(&salt).map_err(|e| StoreError::PasswordHash(e.to_string()))?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| StoreError::PasswordHash(e.to_string()))?;
        Ok(Self(hash.to_string()))
    }

    /// Whether `password` is the one this hash was made from.
    #[must_use]
    pub fn verify(&self, password: &str) -> bool {
        PasswordHash::new(&self.0).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
    }
}

impl std::fmt::Debug for HashedPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HashedPassword(..)")
    }
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

/// A plaintext access token or session secret.
///
/// Only its hash is stored; keep the plaintext to hand to the user (or, for a
/// session, the browser).
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

/// Trim and lowercase `email`, and check that it looks like an email address.
///
/// The check is deliberately loose: one `@` with something on both sides, a
/// `.` in the domain, no spaces or control characters, at most 254 bytes.
///
/// # Errors
///
/// [`StoreError::InvalidEmail`] when it does not look like an email address.
pub fn normalize_email(email: &str) -> Result<String> {
    let email = email.trim().to_lowercase();
    let valid = email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.contains('@')
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if valid {
        Ok(email)
    } else {
        Err(StoreError::InvalidEmail(email))
    }
}

/// The current schema, created on an empty database.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    admin INTEGER NOT NULL DEFAULT 0,
    password_hash TEXT,
    password_temporary INTEGER NOT NULL DEFAULT 0
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
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL
);
PRAGMA user_version = 2;
";

/// Version 1 named users instead of using email addresses and had no
/// passwords or sessions. Its users keep their names and tokens, but need a
/// password set before they can sign in to the web pages.
const MIGRATE_1_TO_2: &str = "
ALTER TABLE users RENAME COLUMN name TO email;
ALTER TABLE users ADD COLUMN password_hash TEXT;
ALTER TABLE users ADD COLUMN password_temporary INTEGER NOT NULL DEFAULT 0;
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL
);
PRAGMA user_version = 2;
";

/// Columns [`user_from_row`] reads, in order.
const USER_COLUMNS: &str = "users.id, users.email, users.admin, users.password_temporary";

fn user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        email: row.get(1)?,
        admin: row.get(2)?,
        must_change_password: row.get(3)?,
    })
}

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
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 1 {
            conn.execute_batch(&format!("BEGIN; {MIGRATE_1_TO_2} COMMIT;"))?;
        }
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

    /// Create the first admin user, on a database with no users.
    ///
    /// # Errors
    ///
    /// [`StoreError::AlreadySetUp`] when a user already exists, or the errors
    /// of [`Self::add_user`].
    pub fn setup(
        &mut self,
        email: &str,
        password: &HashedPassword,
        temporary: bool,
    ) -> Result<User> {
        if self.has_users()? {
            return Err(StoreError::AlreadySetUp);
        }
        self.add_user(email, true, password, temporary)
    }

    /// Create a user. `temporary` marks a password generated for them, which
    /// they must replace when they first sign in.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidEmail`], [`StoreError::UserExists`], or
    /// [`StoreError::Sqlite`].
    pub fn add_user(
        &mut self,
        email: &str,
        admin: bool,
        password: &HashedPassword,
        temporary: bool,
    ) -> Result<User> {
        let email = normalize_email(email)?;
        if self.user(&email)?.is_some() {
            return Err(StoreError::UserExists(email));
        }
        self.conn.execute(
            "INSERT INTO users (email, admin, password_hash, password_temporary)
             VALUES (?1, ?2, ?3, ?4)",
            params![email, admin, password.0, temporary],
        )?;
        Ok(User {
            id: self.conn.last_insert_rowid(),
            email,
            admin,
            must_change_password: temporary,
        })
    }

    /// Look up a user by email address (in any case).
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the query fails.
    pub fn user(&self, email: &str) -> Result<Option<User>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {USER_COLUMNS} FROM users WHERE email = ?1"),
                params![email.trim().to_lowercase()],
                user_from_row,
            )
            .optional()?)
    }

    /// Look up a user by email address, failing when there is none.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnknownUser`] or [`StoreError::Sqlite`].
    pub fn require_user(&self, email: &str) -> Result<User> {
        self.user(email)?
            .ok_or_else(|| StoreError::UnknownUser(email.trim().to_lowercase()))
    }

    /// A user and their password hash, to check a sign-in with
    /// [`HashedPassword::verify`] after releasing the store.
    ///
    /// `None` when there is no such user or they have no password yet.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the query fails.
    pub fn password_of(&self, email: &str) -> Result<Option<(User, HashedPassword)>> {
        let found = self
            .conn
            .query_row(
                &format!("SELECT {USER_COLUMNS}, users.password_hash FROM users WHERE email = ?1"),
                params![email.trim().to_lowercase()],
                |row| Ok((user_from_row(row)?, row.get::<_, Option<String>>(4)?)),
            )
            .optional()?;
        Ok(found.and_then(|(user, hash)| Some((user, HashedPassword(hash?)))))
    }

    /// Replace `user`'s password. `temporary` marks one generated for them.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the update fails.
    pub fn set_password(
        &mut self,
        user: &User,
        password: &HashedPassword,
        temporary: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE users SET password_hash = ?1, password_temporary = ?2 WHERE id = ?3",
            params![password.0, temporary, user.id],
        )?;
        Ok(())
    }

    /// Start a web session for `user` at `now` that lasts `lifetime`, and
    /// return its secret for the browser's cookie.
    ///
    /// Sessions that expired before `now` are removed at the same time.
    ///
    /// # Errors
    ///
    /// [`StoreError::Random`] or [`StoreError::Sqlite`].
    pub fn create_session(
        &mut self,
        user: &User,
        now: OffsetDateTime,
        lifetime: time::Duration,
    ) -> Result<Token> {
        let secret = Token::generate()?;
        self.conn.execute(
            "DELETE FROM sessions WHERE expires_at <= ?1",
            params![now.unix_timestamp()],
        )?;
        self.conn.execute(
            "INSERT INTO sessions (user_id, session_hash, expires_at) VALUES (?1, ?2, ?3)",
            params![user.id, secret.hash(), (now + lifetime).unix_timestamp()],
        )?;
        Ok(secret)
    }

    /// The user signed in with session `secret`, or `None` when it is unknown
    /// or expired at `now`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the query fails.
    pub fn session_user(&self, secret: &Token, now: OffsetDateTime) -> Result<Option<User>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {USER_COLUMNS} FROM sessions JOIN users ON users.id = sessions.user_id
                     WHERE sessions.session_hash = ?1 AND sessions.expires_at > ?2"
                ),
                params![secret.hash(), now.unix_timestamp()],
                user_from_row,
            )
            .optional()?)
    }

    /// End the session with `secret` (signing out). Unknown secrets are ignored.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the delete fails.
    pub fn end_session(&mut self, secret: &Token) -> Result<()> {
        self.conn.execute(
            "DELETE FROM sessions WHERE session_hash = ?1",
            params![secret.hash()],
        )?;
        Ok(())
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
                &format!(
                    "SELECT {USER_COLUMNS} FROM tokens JOIN users ON users.id = tokens.user_id
                     WHERE tokens.token_hash = ?1"
                ),
                params![token.hash()],
                user_from_row,
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

    /// Every user with their grants and token count, sorted by email address.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when a query fails.
    pub fn users(&self) -> Result<Vec<UserSummary>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {USER_COLUMNS}, (SELECT COUNT(*) FROM tokens WHERE user_id = users.id)
             FROM users ORDER BY email"
        ))?;
        let rows = stmt.query_map([], |row| Ok((user_from_row(row)?, row.get::<_, i64>(4)?)))?;
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

    fn password(text: &str) -> HashedPassword {
        ok(HashedPassword::new(text))
    }

    fn add(store: &mut AuthStore, email: &str) -> User {
        ok(store.add_user(email, false, &password("correct horse"), false))
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap_or(OffsetDateTime::UNIX_EPOCH)
    }

    #[test]
    fn setup_creates_one_admin_and_only_once() {
        let mut store = store();
        assert!(!ok(store.has_users()));
        let admin = ok(store.setup("Admin@Example.com", &password("hunter2hunter2"), false));
        assert!(admin.admin);
        assert_eq!(admin.email, "admin@example.com");
        assert_eq!(ok(store.user("ADMIN@example.com")), Some(admin));
        assert!(matches!(
            store.setup("other@example.com", &password("hunter2hunter2"), false),
            Err(StoreError::AlreadySetUp)
        ));
    }

    #[test]
    fn passwords_are_hashed_and_verified() {
        let hash = password("correct horse");
        assert!(hash.verify("correct horse"));
        assert!(!hash.verify("correct horsE"));
        assert!(!format!("{hash:?}").contains("horse"));
        assert!(matches!(
            HashedPassword::new("short"),
            Err(StoreError::WeakPassword)
        ));

        let mut store = store();
        let bob = add(&mut store, "bob@example.com");
        let (found, stored) = ok(store.password_of("bob@example.com")).unwrap_or_else(|| {
            panic!("bob has a password");
        });
        assert_eq!(found, bob);
        assert!(stored.verify("correct horse"));
        assert!(ok(store.password_of("nobody@example.com")).is_none());
    }

    #[test]
    fn temporary_passwords_must_be_changed() {
        let mut store = store();
        let temp = ok(generate_password());
        let carol = ok(store.add_user("carol@example.com", false, &password(&temp), true));
        assert!(carol.must_change_password);
        ok(store.set_password(&carol, &password("my own password"), false));
        let carol = ok(store.require_user("carol@example.com"));
        assert!(!carol.must_change_password);
        let (_, stored) = ok(store.password_of("carol@example.com")).unwrap_or_else(|| {
            panic!("carol has a password");
        });
        assert!(stored.verify("my own password"));
        assert!(!stored.verify(&temp));
    }

    #[test]
    fn generated_passwords_are_grouped_and_distinct() {
        let a = ok(generate_password());
        let b = ok(generate_password());
        assert_ne!(a, b);
        assert_eq!(a.len(), 19);
        let groups: Vec<_> = a.split('-').collect();
        assert_eq!(groups.len(), 4);
        assert!(groups
            .iter()
            .all(|g| g.len() == 4 && g.bytes().all(|c| PASSWORD_ALPHABET.contains(&c))));
    }

    #[test]
    fn sessions_sign_in_until_they_expire_or_end() {
        let mut store = store();
        let bob = add(&mut store, "bob@example.com");
        let secret = ok(store.create_session(&bob, now(), time::Duration::hours(1)));
        assert_eq!(ok(store.session_user(&secret, now())), Some(bob.clone()));
        assert_eq!(
            ok(store.session_user(&secret, now() + time::Duration::hours(2))),
            None
        );
        assert_eq!(
            ok(store.session_user(&Token::from_secret("grit_other"), now())),
            None
        );
        ok(store.end_session(&secret));
        assert_eq!(ok(store.session_user(&secret, now())), None);

        let old = ok(store.create_session(&bob, now(), time::Duration::hours(1)));
        let later = now() + time::Duration::days(1);
        ok(store.create_session(&bob, later, time::Duration::hours(1)));
        assert_eq!(ok(store.session_user(&old, now())), None, "pruned");
    }

    #[test]
    fn tokens_authenticate_their_user() {
        let mut store = store();
        let bob = add(&mut store, "bob@example.com");
        let token = Token::from_secret("grit_bob");
        ok(store.add_token(&bob, &token));
        assert_eq!(ok(store.authenticate(&token)), Some(bob));
        assert_eq!(
            ok(store.authenticate(&Token::from_secret("grit_nope"))),
            None
        );
    }

    #[test]
    fn admins_write_everything_and_users_only_their_grants() {
        let mut store = store();
        let admin = ok(store.setup("admin@example.com", &password("hunter2hunter2"), false));
        let bob = add(&mut store, "bob@example.com");
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
        ok(store.setup("admin@example.com", &password("hunter2hunter2"), false));
        let bob = add(&mut store, "bob@example.com");
        ok(store.add_token(&bob, &Token::from_secret("grit_b1")));
        ok(store.add_token(&bob, &Token::from_secret("grit_b2")));
        ok(store.grant(&bob, "zeta", Access::Read));
        ok(store.grant(&bob, "alpha", Access::Write));

        let users = ok(store.users());
        let emails: Vec<_> = users.iter().map(|u| u.user.email.as_str()).collect();
        assert_eq!(emails, ["admin@example.com", "bob@example.com"]);
        assert_eq!(users[1].tokens, 2);
        let repos: Vec<_> = users[1]
            .grants
            .iter()
            .map(|g| (g.repository.as_str(), g.access))
            .collect();
        assert_eq!(repos, [("alpha", Access::Write), ("zeta", Access::Read)]);
    }

    #[test]
    fn emails_and_repository_names_are_validated() {
        for bad in [
            "bob",
            "bob@",
            "@example.com",
            "bob@localhost",
            "a b@example.com",
            "a@b@c.com",
        ] {
            assert!(
                matches!(normalize_email(bad), Err(StoreError::InvalidEmail(_))),
                "{bad:?}"
            );
        }
        assert_eq!(ok(normalize_email(" Bob@Example.COM ")), "bob@example.com");

        let mut store = store();
        add(&mut store, "bob@example.com");
        assert!(matches!(
            store.add_user("BOB@example.com", true, &password("hunter2hunter2"), false),
            Err(StoreError::UserExists(_))
        ));
        assert!(matches!(
            store.require_user("carol@example.com"),
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
            let admin = ok(store.setup("admin@example.com", &password("hunter2hunter2"), false));
            ok(store.add_token(&admin, &token));
        }
        let store = ok(AuthStore::open(&path));
        assert_eq!(
            ok(store.authenticate(&token)).map(|u| u.email),
            Some("admin@example.com".to_owned())
        );
    }

    #[test]
    fn version_1_databases_are_migrated() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("tempdir: {e}"),
        };
        let path = dir.path().join("serve.db");
        {
            let conn = ok(Connection::open(&path).map_err(StoreError::from));
            let v1 = "
                CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                    admin INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE tokens (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL
                    REFERENCES users(id) ON DELETE CASCADE, token_hash TEXT NOT NULL UNIQUE);
                CREATE TABLE grants (user_id INTEGER NOT NULL REFERENCES users(id)
                    ON DELETE CASCADE, repository TEXT NOT NULL, access TEXT NOT NULL,
                    PRIMARY KEY (user_id, repository));
                INSERT INTO users (name, admin) VALUES ('admin', 1);
                PRAGMA user_version = 1;";
            ok(conn.execute_batch(v1).map_err(StoreError::from));
        }
        let mut store = ok(AuthStore::open(&path));
        let admin = ok(store.require_user("admin"));
        assert!(admin.admin);
        assert!(ok(store.password_of("admin")).is_none());
        ok(store.set_password(&admin, &password("hunter2hunter2"), false));
        assert!(ok(store.password_of("admin")).is_some());
        ok(store.create_session(&admin, now(), time::Duration::hours(1)));
    }
}
