//! Message-level policy for checking objects received from another repository.
//!
//! A server with `receive.fsckObjects` (or a client with `fetch.fsckObjects`)
//! checks every incoming commit, tree and tag with the object fsck rules of
//! [`crate::fsck_standalone`]. Transfer checks are strict: problems Git only
//! warns about in a local `git fsck` (zero-padded file modes, `.` / `..` / `.git`
//! entries, null object ids, ...) refuse the object unless configured otherwise.
//! `<section>.<msg-id>` keys (for example `receive.fsck.missingEmail=warn`)
//! override single messages, and `<section>.skipList` names a file of object ids
//! that are never reported.

use std::collections::{BTreeMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use crate::config::{parse_path, ConfigSet};
use crate::environment::Environment;
use crate::fsck_standalone::{fsck_object_with, FsckError, FsckObjectOptions};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};

/// How one fsck message is treated during a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsckMsgLevel {
    /// The object is refused.
    Error,
    /// The object is accepted; the problem is returned as a warning.
    Warn,
    /// The problem is not reported.
    Ignore,
}

impl FsckMsgLevel {
    /// Parse a configured level: `error`, `warn` or `ignore` (any case).
    ///
    /// # Parameters
    ///
    /// - `value`: the configuration value.
    ///
    /// # Returns
    ///
    /// The level, or `None` for any other value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "ignore" => Some(Self::Ignore),
            _ => None,
        }
    }
}

/// Git's built-in level for an object fsck message id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultFsckLevel {
    /// Always an error; configuration cannot lower it.
    Fatal,
    Error,
    Warn,
    Info,
    Ignore,
}

/// Built-in level for `msg_id` (the camelCase id, any case).
pub(crate) fn default_fsck_level(msg_id: &str) -> DefaultFsckLevel {
    const FATAL: &[&str] = &["nulInHeader", "unterminatedHeader"];
    const WARN: &[&str] = &[
        "emptyName",
        "fullPathname",
        "hasDot",
        "hasDotdot",
        "hasDotgit",
        "nullSha1",
        "zeroPaddedFilemode",
        "nulInCommit",
        "largePathname",
        "sha1Collision",
    ];
    const INFO: &[&str] = &[
        "badFilemode",
        "gitmodulesParse",
        "gitignoreSymlink",
        "gitattributesSymlink",
        "mailmapSymlink",
        "badTagName",
        "missingTaggerEntry",
    ];
    const IGNORE: &[&str] = &["extraHeaderEntry"];
    let is = |ids: &[&str]| ids.iter().any(|id| id.eq_ignore_ascii_case(msg_id));
    if is(FATAL) {
        DefaultFsckLevel::Fatal
    } else if is(WARN) {
        DefaultFsckLevel::Warn
    } else if is(INFO) {
        DefaultFsckLevel::Info
    } else if is(IGNORE) {
        DefaultFsckLevel::Ignore
    } else {
        DefaultFsckLevel::Error
    }
}

/// Which object problems refuse a transfer.
///
/// The default policy is Git's strict transfer policy with no overrides and no
/// skip list. Build one from configuration with [`Self::from_config`] or adjust
/// it with [`Self::set_level`] and [`Self::set_skip_list`], then call
/// [`Self::checker`] once per transfer to check objects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectFsckPolicy {
    /// Overrides keyed by lowercased message id.
    levels: BTreeMap<String, FsckMsgLevel>,
    skip_list: Option<String>,
}

impl ObjectFsckPolicy {
    /// Read message overrides and the skip list from `<section>.*` keys.
    ///
    /// `<section>.<msg-id>` sets the level of one message (`error`, `warn` or
    /// `ignore`; other values are skipped). `<section>.skipList` names the skip
    /// list file. Later entries win, as for any configuration key.
    ///
    /// # Parameters
    ///
    /// - `config`: the loaded configuration.
    /// - `section`: the key prefix without the trailing dot, such as
    ///   `receive.fsck`, `fetch.fsck` or `fsck`.
    ///
    /// # Returns
    ///
    /// The policy those keys describe.
    #[must_use]
    pub fn from_config(config: &ConfigSet, section: &str) -> Self {
        let mut policy = Self::default();
        for entry in config.entries() {
            let Some(name) = entry
                .key
                .strip_prefix(section)
                .and_then(|rest| rest.strip_prefix('.'))
            else {
                continue;
            };
            if name.is_empty() || name.contains('.') {
                continue;
            }
            let Some(value) = entry.value.as_deref() else {
                continue;
            };
            if name == "skiplist" {
                policy.skip_list = Some(value.to_owned());
            } else if let Some(level) = value.split(':').next().and_then(FsckMsgLevel::parse) {
                policy.set_level(name, level);
            }
        }
        policy
    }

    /// Override the level of one message.
    ///
    /// Messages that leave an object unparseable (`nulInHeader`,
    /// `unterminatedHeader`) stay errors whatever level is set.
    ///
    /// # Parameters
    ///
    /// - `msg_id`: the camelCase message id, such as `missingEmail` (any case).
    /// - `level`: how to treat it.
    pub fn set_level(&mut self, msg_id: &str, level: FsckMsgLevel) {
        self.levels.insert(msg_id.to_ascii_lowercase(), level);
    }

    /// The level `msg_id` is treated at under this policy.
    ///
    /// Without an override, Git's built-in level applies with transfer
    /// strictness: warnings are errors and informational messages are warnings.
    ///
    /// # Parameters
    ///
    /// - `msg_id`: the camelCase message id (any case).
    ///
    /// # Returns
    ///
    /// The effective level.
    #[must_use]
    pub fn level(&self, msg_id: &str) -> FsckMsgLevel {
        let default = default_fsck_level(msg_id);
        if let Some(&level) = self.levels.get(&msg_id.to_ascii_lowercase()) {
            if default != DefaultFsckLevel::Fatal || level == FsckMsgLevel::Error {
                return level;
            }
        }
        match default {
            DefaultFsckLevel::Fatal | DefaultFsckLevel::Error | DefaultFsckLevel::Warn => {
                FsckMsgLevel::Error
            }
            DefaultFsckLevel::Info => FsckMsgLevel::Warn,
            DefaultFsckLevel::Ignore => FsckMsgLevel::Ignore,
        }
    }

    /// The configured skip list path, before `~/` expansion.
    #[must_use]
    pub fn skip_list(&self) -> Option<&str> {
        self.skip_list.as_deref()
    }

    /// Set or clear the skip list path.
    ///
    /// # Parameters
    ///
    /// - `path`: a file with one object id per line (`#` starts a comment), or
    ///   `None` for no skip list. `~/` and relative paths are resolved by
    ///   [`Self::checker`].
    pub fn set_skip_list(&mut self, path: Option<String>) {
        self.skip_list = path;
    }

    /// Prepare to check objects: load the skip list.
    ///
    /// # Parameters
    ///
    /// - `hash_algo`: the repository's object format.
    /// - `base_dir`: directory a relative skip list path is resolved against
    ///   (the git directory for a server).
    /// - `env`: environment for `~/` expansion of the skip list path.
    ///
    /// # Returns
    ///
    /// A checker borrowing this policy.
    ///
    /// # Errors
    ///
    /// [`SkipListError`] when the skip list cannot be read or has a line that is
    /// not a full object id.
    pub fn checker(
        &self,
        hash_algo: HashAlgo,
        base_dir: &Path,
        env: &Environment,
    ) -> Result<ObjectFsckChecker<'_>, SkipListError> {
        let skip = match &self.skip_list {
            None => HashSet::new(),
            Some(raw) => load_skip_list(&base_dir.join(parse_path(env, raw)), hash_algo)?,
        };
        Ok(ObjectFsckChecker {
            policy: self,
            skip,
            options: FsckObjectOptions::new(hash_algo),
        })
    }
}

/// Why a skip list could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum SkipListError {
    /// The file could not be read.
    #[error("cannot read fsck skip list {}: {source}", path.display())]
    Read {
        /// The resolved skip list path.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A line is neither blank, a comment, nor a full object id.
    #[error("fsck skip list {}: line {line} is not an object id", path.display())]
    InvalidLine {
        /// The resolved skip list path.
        path: PathBuf,
        /// One-based line number.
        line: usize,
    },
}

fn load_skip_list(path: &Path, hash_algo: HashAlgo) -> Result<HashSet<ObjectId>, SkipListError> {
    let text = std::fs::read_to_string(path).map_err(|source| SkipListError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut oids = HashSet::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let oid = ObjectId::from_hex(line)
            .ok()
            .filter(|oid| oid.algo() == hash_algo)
            .ok_or_else(|| SkipListError::InvalidLine {
                path: path.to_path_buf(),
                line: n + 1,
            })?;
        oids.insert(oid);
    }
    Ok(oids)
}

/// An [`ObjectFsckPolicy`] with its skip list loaded, ready to check objects.
#[derive(Debug, Clone)]
pub struct ObjectFsckChecker<'a> {
    policy: &'a ObjectFsckPolicy,
    skip: HashSet<ObjectId>,
    options: FsckObjectOptions,
}

impl ObjectFsckChecker<'_> {
    /// Check one object against the policy.
    ///
    /// Objects on the skip list and blobs always pass. Checking continues past
    /// warned and ignored problems, so an accepted problem cannot hide a later
    /// error in the same object.
    ///
    /// # Parameters
    ///
    /// - `oid`: the object's id, for the skip list.
    /// - `kind`: the object type.
    /// - `data`: the object body.
    ///
    /// # Returns
    ///
    /// The warning-level problems found, in order (empty for a clean object).
    ///
    /// # Errors
    ///
    /// The first error-level problem.
    pub fn check(
        &self,
        oid: &ObjectId,
        kind: ObjectKind,
        data: &[u8],
    ) -> Result<Vec<FsckError>, FsckError> {
        let mut warnings = Vec::new();
        if self.skip.contains(oid) {
            return Ok(warnings);
        }
        fsck_object_with(kind, data, self.options, &mut |err| {
            match self.policy.level(err.id) {
                FsckMsgLevel::Error => return ControlFlow::Break(()),
                FsckMsgLevel::Warn => warnings.push(err.clone()),
                FsckMsgLevel::Ignore => {}
            }
            ControlFlow::Continue(())
        })?;
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn config(pairs: &[(&str, &str)]) -> ConfigSet {
        let mut config = ConfigSet::new();
        for (k, v) in pairs {
            config.add_command_override(k, v).expect("valid key");
        }
        config
    }

    fn checker(policy: &ObjectFsckPolicy) -> ObjectFsckChecker<'_> {
        policy
            .checker(HashAlgo::Sha1, Path::new("."), &Environment::empty())
            .expect("no skip list")
    }

    fn commit_without_email() -> Vec<u8> {
        format!(
            "tree {TREE}\nauthor Bad Guy no-email 1700000000 +0000\ncommitter C <c@e.com> 1 +0000\n\nbad\n"
        )
        .into_bytes()
    }

    fn tree_entry(mode: &str, name: &str) -> Vec<u8> {
        let mut entry = format!("{mode} {name}\0").into_bytes();
        entry.extend_from_slice(&hex::decode(TREE).expect("hex"));
        entry
    }

    #[test]
    fn transfer_defaults_are_strict() {
        let policy = ObjectFsckPolicy::default();
        assert_eq!(policy.level("missingEmail"), FsckMsgLevel::Error);
        assert_eq!(policy.level("zeroPaddedFilemode"), FsckMsgLevel::Error);
        assert_eq!(policy.level("badFilemode"), FsckMsgLevel::Warn);
        assert_eq!(policy.level("extraHeaderEntry"), FsckMsgLevel::Ignore);
        assert_eq!(policy.level("someFutureId"), FsckMsgLevel::Error);
    }

    #[test]
    fn config_overrides_are_case_insensitive_and_last_wins() {
        let policy = ObjectFsckPolicy::from_config(
            &config(&[
                ("receive.fsck.missingEmail", "ignore"),
                ("receive.fsck.MISSINGEMAIL", "warn"),
                ("receive.fsck.zeroPaddedFilemode", "bogus"),
                ("fsck.badDate", "ignore"),
            ]),
            "receive.fsck",
        );
        assert_eq!(policy.level("missingEmail"), FsckMsgLevel::Warn);
        assert_eq!(policy.level("zeroPaddedFilemode"), FsckMsgLevel::Error);
        assert_eq!(policy.level("badDate"), FsckMsgLevel::Error);
    }

    #[test]
    fn fatal_messages_cannot_be_demoted() {
        let mut policy = ObjectFsckPolicy::default();
        policy.set_level("nulInHeader", FsckMsgLevel::Ignore);
        assert_eq!(policy.level("nulInHeader"), FsckMsgLevel::Error);
    }

    #[test]
    fn large_pathname_level_ignores_length_suffix() {
        let policy = ObjectFsckPolicy::from_config(
            &config(&[("receive.fsck.largePathname", "warn:10")]),
            "receive.fsck",
        );
        assert_eq!(policy.level("largePathname"), FsckMsgLevel::Warn);
    }

    #[test]
    fn check_refuses_errors_and_returns_warnings() {
        let oid = ObjectId::zero();
        let strict = ObjectFsckPolicy::default();
        let err = checker(&strict)
            .check(&oid, ObjectKind::Commit, &commit_without_email())
            .expect_err("missing email is an error");
        assert_eq!(err.id, "missingEmail");

        let mut lenient = ObjectFsckPolicy::default();
        lenient.set_level("missingEmail", FsckMsgLevel::Warn);
        let warnings = checker(&lenient)
            .check(&oid, ObjectKind::Commit, &commit_without_email())
            .expect("warned only");
        assert_eq!(
            warnings.iter().map(|w| w.id).collect::<Vec<_>>(),
            ["missingEmail"]
        );
    }

    #[test]
    fn accepted_problem_does_not_hide_a_later_error() {
        let mut tree = tree_entry("0100644", "a");
        tree.extend(tree_entry("100644", "a"));
        let mut policy = ObjectFsckPolicy::default();
        policy.set_level("zeroPaddedFilemode", FsckMsgLevel::Ignore);
        let err = checker(&policy)
            .check(&ObjectId::zero(), ObjectKind::Tree, &tree)
            .expect_err("duplicate entries still refused");
        assert_eq!(err.id, "duplicateEntries");
    }

    #[test]
    fn ignored_ident_problem_keeps_checking_the_committer() {
        let commit =
            format!("tree {TREE}\nauthor A no-email 1 +0000\ncommitter C <c@e.com> 1 +00\n\n");
        let mut policy = ObjectFsckPolicy::default();
        policy.set_level("missingEmail", FsckMsgLevel::Ignore);
        let err = checker(&policy)
            .check(&ObjectId::zero(), ObjectKind::Commit, commit.as_bytes())
            .expect_err("bad committer timezone");
        assert_eq!(err.id, "badTimezone");
    }

    #[test]
    fn skip_list_objects_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let oid = ObjectId::from_hex("3257000000000000000000000000000000000000").expect("oid");
        std::fs::write(
            dir.path().join("skip"),
            format!("# known bad\n\n{}  # legacy import\n", oid.to_hex()),
        )
        .expect("write");
        let policy = ObjectFsckPolicy::from_config(
            &config(&[("receive.fsck.skipList", "skip")]),
            "receive.fsck",
        );
        assert_eq!(policy.skip_list(), Some("skip"));
        let checker = policy
            .checker(HashAlgo::Sha1, dir.path(), &Environment::empty())
            .expect("skip list loads");
        assert!(checker
            .check(&oid, ObjectKind::Commit, &commit_without_email())
            .is_ok());
        assert!(checker
            .check(
                &ObjectId::zero(),
                ObjectKind::Commit,
                &commit_without_email()
            )
            .is_err());
    }

    #[test]
    fn skip_list_errors_are_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut policy = ObjectFsckPolicy::default();
        policy.set_skip_list(Some("missing".to_owned()));
        assert!(matches!(
            policy.checker(HashAlgo::Sha1, dir.path(), &Environment::empty()),
            Err(SkipListError::Read { .. })
        ));
        std::fs::write(dir.path().join("bad"), "not-an-oid\n").expect("write");
        policy.set_skip_list(Some("bad".to_owned()));
        assert!(matches!(
            policy.checker(HashAlgo::Sha1, dir.path(), &Environment::empty()),
            Err(SkipListError::InvalidLine { line: 1, .. })
        ));
    }
}
