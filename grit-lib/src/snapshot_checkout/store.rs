//! What a server keeps for a repository's snapshot checkouts, under
//! `<git-dir>/grit-serve/checkouts`:
//!
//! - `<id>.json`: each checkout's record ([`StoredCheckout`]): what the
//!   server told the client ([`CheckoutInfo`]) and which branches the
//!   checkout set last. It is only ever replaced whole, through a temporary
//!   file and a rename, so a reader never sees half of one.
//! - `names/<xx>/<rest>`: the work-branch names given out to new checkouts
//!   ([`CheckoutStore::reserve_work_branch`]), one file per name named by the
//!   SHA-1 of the name, so a name is never handed out twice, even across
//!   restarts and before the checkout that got it checks in.
//!
//! [`CheckoutRecord::save`] takes `&mut self`: a server that keeps each
//! checkout's record behind a lock changes it one request at a time.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::objects::{HashAlgo, ObjectId};
use crate::repo::Repository;

use super::{valid_branch_name, CheckoutError, CheckoutInfo, Result};

/// Whether `id` can name a checkout: the 20 lowercase hex digits a server
/// makes, which keeps it a plain file name.
#[must_use]
pub fn valid_checkout_id(id: &str) -> bool {
    id.len() == 20
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A served repository's snapshot-checkout state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutStore {
    dir: PathBuf,
}

/// A work-branch name [`CheckoutStore::reserve_work_branch`] gave out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedName {
    /// The name, reserved for good.
    pub name: String,
    /// The number to start from next time the same name is asked for
    /// (`name-<next>`), so a popular name isn't counted up from the start.
    pub next: u64,
}

impl CheckoutStore {
    /// The store of `repo`: `<git-dir>/grit-serve/checkouts`.
    #[must_use]
    pub fn for_repository(repo: &Repository) -> Self {
        Self::at(repo.git_dir().join("grit-serve").join("checkouts"))
    }

    /// A store kept in `dir`.
    #[must_use]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the repository has snapshot checkouts: the directory exists
    /// once the first one is made.
    #[must_use]
    pub fn has_checkouts(&self) -> bool {
        self.dir.is_dir()
    }

    /// The record of checkout `id`, or `None` when `id` is not a valid id
    /// ([`valid_checkout_id`]). The record need not exist yet.
    #[must_use]
    pub fn record(&self, id: &str) -> Option<CheckoutRecord> {
        valid_checkout_id(id).then(|| CheckoutRecord {
            id: id.to_owned(),
            path: self.dir.join(format!("{id}.json")),
        })
    }

    /// Give out a work-branch name for a new checkout: `wanted`, or
    /// `wanted-2`, `wanted-3`, … (starting at `wanted-<start>` when `start` is
    /// above 1), skipping names the repository has a branch by and names given
    /// out before.
    ///
    /// # Parameters
    ///
    /// - `repo`: the served repository.
    /// - `wanted`: the name the client would like.
    /// - `start`: where to start numbering, from an earlier
    ///   [`ReservedName::next`] for the same `wanted`; 1 to start at `wanted`.
    ///
    /// # Returns
    ///
    /// The reserved name and where to start next time.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::InvalidBranchName`] when `wanted` is not a valid branch
    /// name, and failures writing the reservation.
    pub fn reserve_work_branch(
        &self,
        repo: &Repository,
        wanted: &str,
        start: u64,
    ) -> Result<ReservedName> {
        if !valid_branch_name(wanted) {
            return Err(CheckoutError::InvalidBranchName(wanted.to_owned()));
        }
        let mut i = start.max(1);
        loop {
            let name = if i == 1 {
                wanted.to_owned()
            } else {
                format!("{wanted}-{i}")
            };
            i += 1;
            let has_branch =
                crate::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{name}")).is_ok();
            if !has_branch && self.reserve(&name)? {
                return Ok(ReservedName { name, next: i });
            }
        }
    }

    /// Reserve `name`; `false` when it was reserved before.
    fn reserve(&self, name: &str) -> Result<bool> {
        let key = hex::encode(HashAlgo::Sha1.checksum_digest(name.as_bytes()));
        let path = self.dir.join("names").join(&key[..2]).join(&key[2..]);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(format!("{name}\n").as_bytes())?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// A checkout as the server keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCheckout {
    /// What the server tells the client about the checkout.
    #[serde(flatten)]
    pub info: CheckoutInfo,
    /// The branches this checkout set last, and the commit it set each to,
    /// so it may replace its own work after an amend or rebase.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set_last: BTreeMap<String, String>,
}

impl StoredCheckout {
    /// A new checkout's record: `info`, having set no branch yet.
    #[must_use]
    pub fn new(info: CheckoutInfo) -> Self {
        Self {
            info,
            set_last: BTreeMap::new(),
        }
    }

    /// What this checkout last set `branch` to, if it set it.
    #[must_use]
    pub fn last_set(&self, branch: &str) -> Option<ObjectId> {
        self.set_last
            .get(branch)
            .and_then(|tip| ObjectId::from_hex(tip).ok())
    }

    /// Note that this checkout set (or found) `branch` at `tip`.
    pub fn record_set(&mut self, branch: &str, tip: ObjectId) {
        self.set_last.insert(branch.to_owned(), tip.to_hex());
    }
}

/// The file one checkout's record is kept in.
///
/// Reading takes `&self` and writing `&mut self`, so whoever holds the record
/// (behind a lock, for a server answering requests at once) is the only one
/// changing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRecord {
    id: String,
    path: PathBuf,
}

impl CheckoutRecord {
    /// The checkout's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The record's file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the record.
    ///
    /// # Returns
    ///
    /// `None` when there is no such checkout.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::DamagedRecord`] for a file that is not a record, and
    /// read failures.
    pub fn load(&self) -> Result<Option<StoredCheckout>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| CheckoutError::DamagedRecord {
                id: self.id.clone(),
                path: self.path.clone(),
                detail: e.to_string(),
            })
    }

    /// Replace the record with `checkout`, atomically: it is written to a
    /// temporary file in the same directory and renamed into place.
    ///
    /// # Errors
    ///
    /// Write failures; the old record is then left as it was.
    pub fn save(&mut self, checkout: &StoredCheckout) -> Result<()> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let bytes = serde_json::to_vec_pretty(checkout)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(&format!("{}.", self.id))
            .suffix(".tmp")
            .tempfile_in(dir)?;
        tmp.write_all(&bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::InitOptions;
    use crate::snapshot_checkout::SnapshotRoot;

    fn info(id: &str) -> CheckoutInfo {
        CheckoutInfo {
            id: id.to_owned(),
            name: "demo".to_owned(),
            branch: "main".to_owned(),
            paths: vec!["src".to_owned()],
            base: "1".repeat(40),
            root: "2".repeat(40),
            root_object: "tree …".to_owned(),
            tree: "3".repeat(40),
            roots: vec![SnapshotRoot {
                commit: "2".repeat(40),
                base: "1".repeat(40),
            }],
            files: 3,
            bytes: 12,
            bundle_bytes: 100,
            work: Some("alice/amber-brook".to_owned()),
        }
    }

    #[test]
    fn checkout_ids_are_server_made_hex() {
        assert!(valid_checkout_id("0123456789abcdef0123"));
        assert!(!valid_checkout_id("0123456789ABCDEF0123"));
        assert!(!valid_checkout_id("../../etc/passwd...."));
        assert!(!valid_checkout_id("abc"));
        let store = CheckoutStore::at("/nowhere");
        assert!(store.record("../x").is_none());
        assert_eq!(
            store
                .record("0123456789abcdef0123")
                .map(|r| r.id().to_owned()),
            Some("0123456789abcdef0123".to_owned())
        );
    }

    #[test]
    fn records_round_trip_and_damage_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CheckoutStore::at(dir.path().join("checkouts"));
        assert!(!store.has_checkouts());
        let id = "00112233445566778899";
        let mut record = store.record(id).expect("record");
        assert_eq!(record.load().expect("load"), None);

        let mut checkout = StoredCheckout::new(info(id));
        let tip = ObjectId::from_hex(&"a".repeat(40)).expect("oid");
        checkout.record_set("alice/amber-brook", tip);
        record.save(&checkout).expect("save");
        assert!(store.has_checkouts());
        let loaded = record.load().expect("load").expect("record");
        assert_eq!(loaded, checkout);
        assert_eq!(loaded.last_set("alice/amber-brook"), Some(tip));
        assert_eq!(loaded.last_set("other"), None);
        // The record is the client's view of the checkout plus what it set.
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(record.path()).expect("read")).expect("json");
        assert_eq!(json["id"], id);
        assert_eq!(json["set_last"]["alice/amber-brook"], tip.to_hex());
        // Nothing but the record is left behind.
        let files: Vec<_> = std::fs::read_dir(store.dir())
            .expect("dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from(format!("{id}.json"))]);

        std::fs::write(record.path(), b"{not json").expect("damage");
        assert!(matches!(
            record.load(),
            Err(CheckoutError::DamagedRecord { .. })
        ));
    }

    #[test]
    fn work_branch_names_are_never_given_out_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let store = CheckoutStore::for_repository(&repo);
        let first = store
            .reserve_work_branch(&repo, "bob/calm-fern", 1)
            .expect("reserve");
        assert_eq!(first.name, "bob/calm-fern");
        let second = store
            .reserve_work_branch(&repo, "bob/calm-fern", 1)
            .expect("reserve");
        assert_eq!(second.name, "bob/calm-fern-2");
        assert_eq!(second.next, 3);
        // Kept across a fresh store (a server restart).
        let again = CheckoutStore::for_repository(&repo)
            .reserve_work_branch(&repo, "bob/calm-fern", 1)
            .expect("reserve");
        assert_eq!(again.name, "bob/calm-fern-3");

        // A name the repository has a branch by is numbered too.
        let commit = repo
            .odb()
            .write(
                crate::objects::ObjectKind::Commit,
                format!(
                    "tree {}\nauthor A <a@e> 1 +0000\ncommitter A <a@e> 1 +0000\n\nx\n",
                    repo.odb()
                        .write(crate::objects::ObjectKind::Tree, b"")
                        .expect("tree")
                )
                .as_bytes(),
            )
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/alice/x", &commit).expect("ref");
        assert_eq!(
            store
                .reserve_work_branch(&repo, "alice/x", 1)
                .expect("reserve")
                .name,
            "alice/x-2"
        );
        assert!(matches!(
            store.reserve_work_branch(&repo, "bad..name", 1),
            Err(CheckoutError::InvalidBranchName(_))
        ));
    }
}
