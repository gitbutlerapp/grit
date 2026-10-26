//! Test helpers shared by the snapshot checkout's client-side tests: a
//! served repository that makes snapshots of `src`, and checkouts of it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle::{write_bundle, BundleSpec};
use crate::config::environment::RepositoryOptions;
use crate::objects::ObjectId;
use crate::progress::NeverCancel;
use crate::remote::fetch::NoProgress;
use crate::repo::Repository;

use super::{make_snapshot, materialize, target_ref, CheckoutInfo, CheckoutMarker, SnapshotRoot};

pub(crate) const ID: &str = "0123456789abcdef0123";

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

pub(crate) fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
}

pub(crate) fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex).expect("oid")
}

pub(crate) fn commit(dir: &Path, path: &str, text: &str, message: &str) -> ObjectId {
    let file = dir.join(path);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(file, text).expect("write");
    git(dir, &["add", path]);
    git(dir, &["commit", "-qm", message]);
    oid(&git(dir, &["rev-parse", "HEAD"]))
}

/// A served repository and the snapshots of `src` it made for one checkout.
pub(crate) struct Served {
    pub(crate) dir: PathBuf,
    pub(crate) roots: Vec<SnapshotRoot>,
    /// The checkout as the server has it after its last snapshot.
    last: Option<CheckoutInfo>,
}

impl Served {
    pub(crate) fn new(root: &Path) -> Self {
        let dir = root.join("served");
        std::fs::create_dir_all(&dir).expect("mkdir");
        git(&dir, &["init", "-q", "-b", "main"]);
        for (path, text) in [
            ("README", "top\n"),
            ("src/a.rs", "a\n"),
            ("src/b.rs", "b\n"),
            ("docs/d.md", "d\n"),
        ] {
            commit(&dir, path, text, path);
        }
        Self {
            dir,
            roots: Vec::new(),
            last: None,
        }
    }

    pub(crate) fn url(&self) -> String {
        self.dir.display().to_string()
    }

    /// A snapshot of `main` on top of the last one, as the server's update makes it.
    pub(crate) fn snapshot(&mut self) -> CheckoutInfo {
        let repo = open(&self.dir);
        let base = oid(&git(&self.dir, &["rev-parse", "main"]));
        let parent = self.roots.last().map(|r| oid(&r.commit));
        let paths = vec!["src".to_owned()];
        let snap = make_snapshot(&repo, base, &paths, parent).expect("snapshot");
        crate::refs::write_ref(repo.git_dir(), &target_ref(ID), &snap.commit).expect("target ref");
        self.roots.push(SnapshotRoot {
            commit: snap.commit.to_hex(),
            base: base.to_hex(),
        });
        let info = CheckoutInfo {
            id: ID.to_owned(),
            name: "served".to_owned(),
            branch: "main".to_owned(),
            paths,
            base: base.to_hex(),
            root: snap.commit.to_hex(),
            root_object: String::from_utf8_lossy(&snap.object).into_owned(),
            tree: snap.tree.to_hex(),
            roots: self.roots.clone(),
            files: snap.files,
            bytes: snap.bytes,
            bundle_bytes: 0,
            work: Some("work".to_owned()),
        };
        self.last = Some(info.clone());
        info
    }

    /// The checkout as the server has it now, without a new snapshot.
    pub(crate) fn info(&self) -> CheckoutInfo {
        self.last.clone().expect("a snapshot was made")
    }

    /// A commit changing `path` on the served branch `branch` (made from
    /// `main` when it doesn't exist), leaving `HEAD` on `main`.
    pub(crate) fn commit_on(
        &self,
        branch: &str,
        path: &str,
        text: &str,
        message: &str,
    ) -> ObjectId {
        let exists = Command::new("git")
            .current_dir(&self.dir)
            .args([
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{branch}"),
            ])
            .output()
            .expect("run git")
            .status
            .success();
        if exists {
            git(&self.dir, &["switch", "-q", branch]);
        } else {
            git(&self.dir, &["switch", "-q", "-c", branch]);
        }
        let made = commit(&self.dir, path, text, message);
        git(&self.dir, &["switch", "-q", "main"]);
        made
    }

    /// Commit on the served `main` and snapshot it.
    pub(crate) fn moves(&mut self, path: &str, text: &str) -> CheckoutInfo {
        commit(&self.dir, path, text, &format!("server changes {path}"));
        self.snapshot()
    }
}

/// A checkout of `served`'s first snapshot at `at/co`, on branch `work`.
pub(crate) fn check_out(served: &mut Served, at: &Path) -> (PathBuf, Repository) {
    let info = served.snapshot();
    let bundle = at.join("snapshot.bundle");
    let mut out = std::fs::File::create(&bundle).expect("bundle file");
    write_bundle(
        &open(&served.dir),
        &BundleSpec {
            include: vec![(oid(&info.root), "refs/heads/main".to_owned())],
            exclude: Vec::new(),
            filter: None,
        },
        &mut out,
    )
    .expect("bundle");
    let dest = at.join("co");
    let made = materialize(
        &dest,
        &info,
        &bundle,
        &served.url(),
        "work",
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("materialize");
    git(&dest, &["config", "user.name", "T"]);
    git(&dest, &["config", "user.email", "t@example.com"]);
    (dest, made.repo)
}

pub(crate) fn marker(repo: &Repository) -> CheckoutMarker {
    CheckoutMarker::read(repo.git_dir()).expect("a checkout")
}

pub(crate) fn git_output_fails(dir: &Path, args: &[&str]) -> bool {
    !Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git")
        .status
        .success()
}
