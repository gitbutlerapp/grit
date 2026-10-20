// API docs: https://docs.rs/grit-lib/latest/grit_lib/diff/index.html
use grit_lib::diff::{resolve_diff_arguments, DiffOptions, DiffTarget};
use grit_lib::repo::{InitOptions, Repository};

fn main() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let args = resolve_diff_arguments(&repo, None, false, false, None, &[], &[])?;
    assert!(matches!(args.target, DiffTarget::WorktreeVsHead));
    let opts = DiffOptions::default();
    let _files = repo.diff().files(&args.target, &args.pathspecs, &opts)?;
    Ok(())
}
