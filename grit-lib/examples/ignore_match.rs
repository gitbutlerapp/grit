//! Evaluate `.gitignore` rules using [`ignore::IgnoreMatcher::check_path`].
//!
//! Run: `cargo run -p grit-lib --example ignore_match`

use grit_lib::ignore::IgnoreMatcher;
use grit_lib::repo::init_repository;
use std::fs;

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(
        root.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )?;

    let wt = repo.work_tree.as_ref().expect("non-bare");
    fs::write(wt.join(".gitignore"), "*.log\n")?;

    let mut matcher = IgnoreMatcher::from_repository(&repo)?;
    let (ignored_log, m_log) = matcher.check_path(&repo, None, "build.log", false)?;
    let (ignored_txt, m_txt) = matcher.check_path(&repo, None, "readme.txt", false)?;

    println!("build.log ignored={ignored_log} ({m_log:?})");
    println!("readme.txt ignored={ignored_txt} ({m_txt:?})");

    Ok(())
}
