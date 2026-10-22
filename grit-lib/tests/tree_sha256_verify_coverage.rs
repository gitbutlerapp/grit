//! Coverage for tree-sha256 tag verification public API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::commit::signing::GpgConfig;
use grit_lib::config::environment::RepositoryOptions;
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
        TagVerification {
            signature: SignatureVerdict::NotChecked,
            tree_sha256: None,
        },
        TreeSha256Verdict {
            claimed: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap(),
            computed: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap(),
            excluded: vec![],
            bogus_excluded: vec![],
            cycles: vec![],
            files: 0,
            submodules: 0,
        },
        TreeSha256TagHeaders {
            target: "0000000000000000000000000000000000000001".parse().unwrap(),
            claimed: None,
            excluded: vec![],
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path(), &grit_lib::repo::InitOptions::default()).unwrap();
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
