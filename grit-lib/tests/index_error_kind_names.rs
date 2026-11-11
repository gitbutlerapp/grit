//! Pin delegated [`IndexError::kind_name`] slugs preserved for `--json` output.

use grit_lib::index::IndexError;

const INDEX_KIND_NAMES: &[&str] = &[
    "index_error",
    "index_unmerged",
    "index_path_prefix_conflict",
    "index_fsmonitor_extension_truncated",
    "index_fsmonitor_extension_bad_version",
    "index_fsmonitor_extension_ewah_invalid",
    "cache_tree_corrupt",
    "lock_held",
    "git_dir_path_io",
    "message",
];

#[test]
fn index_error_kind_names_are_stable() {
    for slug in INDEX_KIND_NAMES {
        assert!(
            sample_for_kind(slug).kind_name() == *slug,
            "expected kind {slug}"
        );
    }
}

fn sample_for_kind(slug: &str) -> IndexError {
    use grit_lib::error::{GitDirPathOperation, LockHeldError, PathIoError};
    use grit_lib::index::{IndexChmodReason, IndexCorrupt, IndexCorruptReason};
    use std::path::PathBuf;

    match slug {
        "index_error" => IndexError::Corrupt {
            corrupt: IndexCorrupt::new(IndexCorruptReason::BadSignature),
            detail: "bad magic: expected DIRC".into(),
        },
        "index_unmerged" => IndexError::unmerged(),
        "index_path_prefix_conflict" => IndexError::PathPrefixConflict {
            directory: PathBuf::from("a"),
            file: PathBuf::from("a/b"),
        },
        "index_fsmonitor_extension_truncated" => IndexError::FsmonitorExtensionTruncated,
        "index_fsmonitor_extension_bad_version" => IndexError::FsmonitorExtensionBadVersion(9),
        "index_fsmonitor_extension_ewah_invalid" => IndexError::FsmonitorExtensionEwahInvalid,
        "cache_tree_corrupt" => IndexError::CacheTreeCorrupt,
        "lock_held" => IndexError::LockHeld(LockHeldError::new(".git/index.lock", None)),
        "git_dir_path_io" => IndexError::PathIo(PathIoError::git_dir(
            ".git/index",
            GitDirPathOperation::WriteIndex,
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )),
        "message" => IndexError::Chmod {
            path: "f.txt".into(),
            reason: IndexChmodReason::UnmergedPath,
        },
        other => panic!("unknown slug {other}"),
    }
}
