//! grit-lib 0.6 API overview — compiled example for the library guide.
//!
//! Opens a temporary repository, exercises core handles, and demonstrates typed errors.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{DiffOptions, DiffRequest};
use grit_lib::error::{Error, ObjectError};
use grit_lib::objects::{ObjectId, ObjectKind, TreeEntry};
use grit_lib::repo::{InitOptions, Repository};

fn describe_missing_object(err: &Error) -> &'static str {
    match err {
        Error::Object(ObjectError::NotFound { .. }) => "object_not_found",
        _ => "other",
    }
}

fn main() -> grit_lib::error::Result<()> {
    let temp = tempfile::tempdir().map_err(Error::Io)?;
    let init = InitOptions::default().with_repository_options(RepositoryOptions::empty());
    let (repo, _) = Repository::init(temp.path(), &init)?;

    let blob_oid = repo
        .objects()
        .write(ObjectKind::Blob, b"grit-lib 0.6 API tour\n")?;
    let read_back = repo.objects().read(&blob_oid)?;
    assert_eq!(read_back.kind, ObjectKind::Blob);

    let _tree_oid = repo.objects().write_tree([TreeEntry {
        mode: 0o100644,
        name: b"README".to_vec(),
        oid: blob_oid,
    }])?;

    let heads = repo.references().list("refs/heads/")?;
    println!("local_branches={}", heads.len());

    let args = repo.diff().resolve_request(None, &DiffRequest::default())?;
    let _diff = repo
        .diff()
        .files(&args.target, &args.pathspec, &DiffOptions::default())?;

    let missing = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")?;
    let missing_err = repo.objects().read(&missing).unwrap_err();
    println!(
        "blob={blob_oid} missing_object={}",
        describe_missing_object(&missing_err)
    );
    Ok(())
}
