//! Crafted tree objects: grit fsck vs system `git fsck` on hostile entry names.

use grit_lib::maintenance::fsck::{fsck_object, FsckObjectOptions};
use grit_lib::maintenance::{fsck, FsckOptions};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::path_validate::{validate_path, EntryMode, PathValidationKind, ProtectOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{
    git_fsck, git_hash_object_literally, write_loose_object, HashAlgo as FixtureAlgo, RepoFixture,
};

const SHA1_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn tree_body(entry_name: &[u8], mode: &str) -> Vec<u8> {
    let oid = hex::decode(SHA1_TREE).expect("hex");
    let mut out = format!("{mode} ").into_bytes();
    out.extend_from_slice(entry_name);
    out.push(0);
    out.extend_from_slice(&oid);
    out
}

struct HostileName {
    label: &'static str,
    mode: &'static str,
    entry: &'static [u8],
}

fn hostile_names() -> Vec<HostileName> {
    vec![
        HostileName {
            label: "dot",
            entry: b".",
            mode: "100644",
        },
        HostileName {
            label: "dotdot",
            entry: b"..",
            mode: "100644",
        },
        HostileName {
            label: "dotgit",
            entry: b".git",
            mode: "100644",
        },
        HostileName {
            label: "git tilde1",
            entry: b"git~1",
            mode: "100644",
        },
        HostileName {
            label: "ntfs trailing dot",
            entry: b".git.",
            mode: "100644",
        },
        HostileName {
            label: "ntfs backslash dotgit",
            entry: b"foo\\.git",
            mode: "100644",
        },
        HostileName {
            label: "hfs dotgit",
            entry: ".gI\u{200c}T".as_bytes(),
            mode: "100644",
        },
        HostileName {
            label: "slash in name",
            entry: b"foo/bar",
            mode: "100644",
        },
        HostileName {
            label: "safe name",
            entry: b"readme.txt",
            mode: "100644",
        },
        HostileName {
            label: "utf8 multibyte safe",
            entry: "abcde日.txt".as_bytes(),
            mode: "100644",
        },
    ]
}

const PATH_FSCK_IDS: &[&str] = &[
    "hasDot",
    "hasDotdot",
    "hasDotgit",
    "fullPathname",
    "gitmodulesSymlink",
    "gitattributesSymlink",
    "gitignoreSymlink",
    "mailmapSymlink",
];

fn grit_object_fsck_rejects(body: &[u8]) -> bool {
    fsck_object(
        ObjectKind::Tree,
        body,
        FsckObjectOptions::new(HashAlgo::Sha1),
    )
    .is_err()
}

fn grit_repo_fsck_rejects(body: &[u8]) -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    write_loose_object(
        repo.git_dir().join("objects").as_path(),
        FixtureAlgo::Sha1,
        "tree",
        body,
    )
    .expect("write tree");
    let report = fsck(&repo, &FsckOptions::objects_and_refs(true)).expect("fsck");
    !report.ok
        && report.issues.iter().any(|i| {
            PATH_FSCK_IDS
                .iter()
                .any(|id| i.msg_id.eq_ignore_ascii_case(id))
        })
}

fn git_tree_fsck_rejects(body: &[u8]) -> bool {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    git_hash_object_literally(repo.path(), FixtureAlgo::Sha1, "tree", body).expect("hash-object");
    let fsck = git_fsck(repo.path(), true);
    if fsck.ok {
        return false;
    }
    fsck.msg_ids
        .iter()
        .any(|id| PATH_FSCK_IDS.contains(&id.as_str()))
}

#[test]
fn path_validate_fsck_agrees_with_git_on_hostile_tree_names() {
    for case in hostile_names() {
        let body = tree_body(case.entry, case.mode);
        let git_rejects = git_tree_fsck_rejects(&body);
        let grit_object = grit_object_fsck_rejects(&body);
        let grit_repo = grit_repo_fsck_rejects(&body);
        assert_eq!(
            git_rejects, grit_object,
            "{} object fsck: git={git_rejects} grit={grit_object}",
            case.label
        );
        assert_eq!(
            git_rejects, grit_repo,
            "{} repo fsck: git={git_rejects} grit={grit_repo}",
            case.label
        );
    }
}

#[test]
fn symlink_dot_special_repo_fsck_matches_git() {
    for (label, entry) in [
        ("gitmodules symlink", b".gitmodules".as_slice()),
        ("gitignore symlink", b".gitignore".as_slice()),
    ] {
        let body = tree_body(entry, "120000");
        let git_rejects = git_tree_fsck_rejects(&body);
        let grit_repo = grit_repo_fsck_rejects(&body);
        assert_eq!(
            git_rejects, grit_repo,
            "{label}: git={git_rejects} grit repo fsck={grit_repo}"
        );
    }
}

#[test]
fn validate_path_empty_separators_match_git_update_index_rejection() {
    let opts = ProtectOptions::default();
    for path in ["a/", "/a", "a//b"] {
        assert!(
            validate_path(
                path.as_bytes(),
                EntryMode::new(0o100644),
                opts,
                PathValidationKind::Worktree,
            )
            .is_err(),
            "{path} should be rejected"
        );
        let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
        let out = std::process::Command::new("git")
            .current_dir(repo.path())
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                "100644",
                SHA1_TREE,
                path,
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("update-index");
        assert!(
            !out.status.success(),
            "git should reject path {path}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(validate_path(
        b"a/b",
        EntryMode::new(0o100644),
        opts,
        PathValidationKind::Worktree,
    )
    .is_ok());
}
