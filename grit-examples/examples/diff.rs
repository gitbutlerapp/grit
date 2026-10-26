// API docs: https://docs.rs/grit-lib/latest/grit_lib/diff/index.html
use grit_lib::diff::{resolve_diff_request, DiffOptions, DiffRequest, DiffTarget};
use grit_lib::repo::{InitOptions, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let args = resolve_diff_request(&repo, None, &DiffRequest::default())?;
    assert!(matches!(args.target, DiffTarget::WorktreeVsHead));
    let opts = DiffOptions::default();
    let _files = repo.diff().files(&args.target, &args.pathspecs, &opts)?;
    Ok(())
}
