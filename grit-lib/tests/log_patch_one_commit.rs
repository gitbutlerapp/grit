//! One-off parity helper (run with `GRIT_LOG_PATCH_OID=<hex>`).

use std::io::Cursor;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    write_commit_log_patch, LogPatchCommitHeader, LogPatchDiffOptions, LogPatchOptions,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_in(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn log_patch_one_commit_env() {
    let Ok(oid_hex) = std::env::var("GRIT_LOG_PATCH_OID") else {
        return;
    };
    let corpus = if let Ok(p) = std::env::var("GRIT_LOG_PATCH_GIT_GIT") {
        std::path::PathBuf::from(p)
    } else {
        std::path::PathBuf::from("/tmp/grit-bench-odb-cache/git.git")
    };
    assert!(corpus.is_dir(), "git.git corpus directory missing");
    let oid = ObjectId::from_hex(oid_hex.trim()).expect("hex oid");
    let git_out = git_in(
        corpus.as_path(),
        &[
            "log",
            "-1",
            "-p",
            "--no-color",
            "--format=%H",
            &oid.to_hex(),
        ],
    );
    let repo = Repository::discover(Some(&corpus), &RepositoryOptions::empty()).expect("open");
    let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
    let patch_opts = LogPatchOptions {
        diff: LogPatchDiffOptions::default(),
        commit_header: LogPatchCommitHeader::FormatPercentH,
        ..LogPatchOptions::git_default(&config)
    };
    let mut out = Cursor::new(Vec::new());
    repo.odb()
        .with_pack_read_context(|| write_commit_log_patch(&repo, &mut out, &oid, &patch_opts))
        .expect("patch");
    let grit_out = out.into_inner();
    if grit_out != git_out {
        let pos = grit_out
            .iter()
            .zip(git_out.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        std::fs::write("/tmp/grit-one.patch", &grit_out).ok();
        std::fs::write("/tmp/git-one.patch", &git_out).ok();
        panic!("patch mismatch for {} at byte {pos}", oid.to_hex());
    }
}
