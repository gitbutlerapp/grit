//! Streamed tracked staging interoperates with Git for both object formats.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

#[test]
fn size_changed_stream_round_trips_with_system_git() {
    for format in ["sha1", "sha256"] {
        for threshold in ["1m", "512m"] {
            let dir = TempDir::new().expect("tempdir");
            let root = dir.path();
            git(
                root,
                &[
                    "init",
                    "-q",
                    "-b",
                    "main",
                    &format!("--object-format={format}"),
                ],
            );
            git(root, &["config", "core.bigFileThreshold", threshold]);
            git(root, &["config", "core.autocrlf", "true"]);
            let mut bytes = vec![b'x'; 2 * 1024 * 1024];
            bytes[0] = 0;
            fs::write(root.join("large.bin"), &bytes).expect("write");
            git(root, &["add", "large.bin"]);
            git(root, &["commit", "-qm", "initial"]);
            let repo = Repository::open(
                &root.join(".git"),
                &RepositoryOptions::empty().with_work_tree(root),
            )
            .expect("open");
            bytes.extend_from_slice(b"changed\r\n");
            fs::write(root.join("large.bin"), &bytes).expect("modify");
            stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
            let expected = git(root, &["hash-object", "large.bin"]);
            let staged = git(root, &["rev-parse", ":large.bin"]);
            assert_eq!(staged.trim(), expected.trim());
            git(root, &["fsck", "--strict"]);
            bytes.extend_from_slice(b"git update\n");
            fs::write(root.join("large.bin"), &bytes).expect("modify again");
            git(root, &["add", "large.bin"]);
            let index = grit_lib::index::Index::load_expand_sparse(
                &repo.git_dir().join("index"),
                repo.odb(),
            )
            .expect("read Git index");
            let entry = index
                .entries()
                .iter()
                .find(|entry| entry.path == b"large.bin")
                .expect("entry");
            assert_eq!(
                repo.odb().read(&entry.oid).expect("read Git blob").data,
                bytes
            );
        }
    }
}
