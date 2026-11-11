//! Coverage for tree-sha256 tag verification public API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::commit::signing::GpgConfig;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::tag::{TagError, VerifiedTag};
use grit_lib::repo::Repository;
use grit_lib::tree_sha256::{
    parse_tree_sha256_tag_headers, verify_tag, SignatureVerdict, TagVerification, TagVerifyOptions,
    TreeSha256TagHeaders, TreeSha256Verdict, TREE_SHA256_EXCLUDED_HEADER, TREE_SHA256_HEADER,
};

#[test]
fn public_tag_verify_types() {
    let _ = (
        TREE_SHA256_HEADER,
        TREE_SHA256_EXCLUDED_HEADER,
        TagVerifyOptions {
            check_signature: false,
            gpg: GpgConfig::from_config(&grit_lib::config::ConfigSet::default()).unwrap(),
        },
        TagVerification::new(SignatureVerdict::NotChecked, None, vec![]),
        {
            let mut verdict = TreeSha256Verdict::new(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .parse()
                    .unwrap(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .parse()
                    .unwrap(),
            );
            verdict
        },
        TreeSha256TagHeaders::new(
            "0000000000000000000000000000000000000001".parse().unwrap(),
            None,
            vec![],
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &grit_lib::repo::InitOptions::default()).unwrap();
    let _ = parse_tree_sha256_tag_headers(b"object 0000000000000000000000000000000000000001\n\n");
    let _ = verify_tag(
        &repo,
        "0000000000000000000000000000000000000001".parse().unwrap(),
        &TagVerifyOptions {
            check_signature: false,
            gpg: GpgConfig::from_config(&grit_lib::config::ConfigSet::default()).unwrap(),
        },
    );
    let _ = Repository::discover(Some(dir.path()), &RepositoryOptions::empty());
}

#[test]
fn public_verify_tag_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &grit_lib::repo::InitOptions::default()).unwrap();
    let options = TagVerifyOptions {
        check_signature: false,
        gpg: GpgConfig::from_config(&grit_lib::config::ConfigSet::default()).unwrap(),
    };
    let err = repo.tags().verify("missing", &options).unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::Tag(TagError::NotFound { .. })
    ));
    let lightweight = TagError::Lightweight { name: "v1".into() };
    assert_eq!(lightweight.to_string(), "tag 'v1' is lightweight");
    let _ = VerifiedTag::new(
        "v1",
        "0000000000000000000000000000000000000001".parse().unwrap(),
        "0000000000000000000000000000000000000002".parse().unwrap(),
        TagVerification::new(SignatureVerdict::NotChecked, None, vec![]),
    );
}
