//! In-process `upload_pack` advertisement to exercise `serve/upload_pack.rs` coverage.

use std::io::Cursor;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectKind;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::serve::{upload_pack, ProtocolVersion, ServeError, ServeOptions};
use grit_lib::repo::{InitOptions, Repository};

#[test]
fn upload_pack_v0_and_v2_advertise_refs_only() -> grit_lib::remote::serve::Result<()> {
    let dir = tempfile::tempdir().map_err(ServeError::Io)?;
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .map_err(ServeError::Repository)?;
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty())
        .map_err(ServeError::Repository)?;
    let tree = repo.odb().write(ObjectKind::Tree, b"")?;
    let commit_body =
        format!("tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\ninit\n");
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())?;
    std::fs::write(
        repo.git_dir().join("refs/heads/main"),
        format!("{commit}\n"),
    )
    .map_err(ServeError::Io)?;

    for protocol in [ProtocolVersion::V0, ProtocolVersion::V2] {
        let mut opts = ServeOptions {
            protocol,
            stateless_rpc: false,
            advertise_refs: true,
            agent: "grit-coverage".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        };
        let mut out = Vec::new();
        upload_pack(&repo, &mut Cursor::new(b""), &mut out, &mut opts)?;
        assert!(!out.is_empty());
    }
    Ok(())
}
