//! In-process `upload_pack` advertisement to exercise `serve/upload_pack.rs` coverage.

use std::io::Cursor;
use std::process::Command;

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

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn v0_advertised_ref_names(out: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut pos = 0;
    while pos + 4 <= out.len() {
        let total_len =
            usize::from_str_radix(std::str::from_utf8(&out[pos..pos + 4]).unwrap(), 16).unwrap();
        if total_len == 0 {
            break;
        }
        pos += 4;
        let payload = &out[pos..pos + total_len - 4];
        pos += total_len - 4;
        let text = std::str::from_utf8(payload).unwrap().trim_end_matches('\n');
        let Some((_, rest)) = text.split_once(' ') else {
            continue;
        };
        let name = rest.split('\0').next().unwrap_or(rest);
        if name.ends_with("^{}") || name == "capabilities^{}" {
            continue;
        }
        names.push(name.to_owned());
    }
    names.sort();
    names
}

fn grit_v0_advertisement(repo: &Repository) -> Vec<u8> {
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: false,
        advertise_refs: true,
        hidden_refs: Vec::new(),
        ..ServeOptions::default()
    };
    let mut out = Vec::new();
    upload_pack(repo, &mut Cursor::new(b""), &mut out, &mut opts).expect("upload_pack");
    out
}

#[test]
fn modest_packed_refs_v0_advertisement_matches_system_git() -> grit_lib::remote::serve::Result<()> {
    if !git_available() {
        return Ok(());
    }
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
    std::fs::write(
        repo.git_dir().join("refs/heads/topic"),
        format!("{commit}\n"),
    )
    .map_err(ServeError::Io)?;
    std::fs::write(
        repo.git_dir().join("packed-refs"),
        format!(
            "# pack-refs with: peeled fully-peeled sorted\n{commit} refs/heads/main\n{commit} refs/heads/topic\n"
        ),
    )
    .map_err(ServeError::Io)?;
    let _ = std::fs::remove_file(repo.git_dir().join("refs/heads/main"));
    let _ = std::fs::remove_file(repo.git_dir().join("refs/heads/topic"));

    let git_out = Command::new("git")
        .current_dir(repo.git_dir())
        .args(["upload-pack", "--advertise-refs", "."])
        .output()
        .expect("git upload-pack");
    assert!(
        git_out.status.success(),
        "{}",
        String::from_utf8_lossy(&git_out.stderr)
    );

    let grit_names = v0_advertised_ref_names(&grit_v0_advertisement(&repo));
    let git_names = v0_advertised_ref_names(&git_out.stdout);
    assert_eq!(grit_names, git_names);
    Ok(())
}

#[test]
fn lying_sorted_packed_refs_v0_advertisement_matches_system_git(
) -> grit_lib::remote::serve::Result<()> {
    if !git_available() {
        return Ok(());
    }
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
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")
        .map_err(ServeError::Io)?;
    std::fs::write(
        repo.git_dir().join("packed-refs"),
        format!(
            "# pack-refs with: peeled fully-peeled sorted\n\
             {commit} refs/heads/a\n\
             {commit} refs/heads/b\n\
             {commit} refs/heads/d\n\
             {commit} refs/heads/c\n\
             {commit} refs/heads/z\n"
        ),
    )
    .map_err(ServeError::Io)?;

    let git_out = Command::new("git")
        .current_dir(repo.git_dir())
        .args(["upload-pack", "--advertise-refs", "."])
        .output()
        .expect("git upload-pack");
    assert!(
        git_out.status.success(),
        "{}",
        String::from_utf8_lossy(&git_out.stderr)
    );

    let grit_names = v0_advertised_ref_names(&grit_v0_advertisement(&repo));
    let git_names = v0_advertised_ref_names(&git_out.stdout);
    assert_eq!(grit_names, git_names);
    for name in [
        "refs/heads/a",
        "refs/heads/b",
        "refs/heads/c",
        "refs/heads/d",
    ] {
        assert_eq!(
            grit_names.iter().filter(|n| n.as_str() == name).count(),
            1,
            "duplicate {name} in grit advertisement: {grit_names:?}"
        );
    }
    Ok(())
}
