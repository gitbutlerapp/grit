//! Smoke coverage for [`FilesystemCapabilities`] and related init options.

use grit_lib::config::init_filesystem::{
    FilesystemCapabilities, FilesystemCapabilitiesError, InitFilesystemConfigOptions,
    PrecomposeDetection,
};
use std::fs;
use tempfile::TempDir;

#[test]
fn filesystem_capabilities_public_api_smoke() {
    let td = TempDir::new().expect("tempdir");
    let git_dir = td.path().join(".git");
    fs::create_dir_all(&git_dir).expect("mkdir");
    fs::write(git_dir.join("config"), "[core]\n").expect("config");

    let _ = FilesystemCapabilitiesError::Io(std::io::Error::other("smoke"));
    let _opts = InitFilesystemConfigOptions::default()
        .with_precompose_detection(PrecomposeDetection::Platform);
    let caps = FilesystemCapabilities::detect(&git_dir).expect("detect");
    assert_eq!(
        FilesystemCapabilities::detect_case_insensitive(&git_dir).expect("case"),
        caps.case_insensitive
    );
    assert_eq!(
        FilesystemCapabilities::detect_trust_filemode(&git_dir).expect("filemode"),
        caps.trust_filemode
    );
    assert_eq!(
        FilesystemCapabilities::detect_symlinks_supported(&git_dir).expect("symlinks"),
        caps.symlinks
    );
    assert_eq!(
        FilesystemCapabilities::detect_precomposes_unicode(&git_dir).expect("precompose"),
        caps.precomposes_unicode
    );
}
