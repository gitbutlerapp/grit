//! In-process `upload_pack` advertisement to exercise `serve/upload_pack.rs` coverage.

use std::io::Cursor;
use std::process::Command;

use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectKind;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefName, RefStore, RefTransaction, RefUpdate,
};
use grit_lib::remote::serve::{ProtocolVersion, ServeError, ServeOptions, UploadPack};
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
    )?;
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
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
        UploadPack::new(&repo, &mut opts).run(&mut Cursor::new(b""), &mut out)?;
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
    UploadPack::new(repo, &mut opts)
        .run(&mut Cursor::new(b""), &mut out)
        .expect("upload_pack");
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
    )?;
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
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
    )?;
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
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

fn pkt_line(line: &str) -> Vec<u8> {
    let payload = format!("{line}\n");
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

fn memory_serve_repo(
    dir: &std::path::Path,
) -> grit_lib::remote::serve::Result<(Repository, Arc<MemoryRefStore>, grit_lib::objects::ObjectId)>
{
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("master"),
    )?;
    let store = Arc::new(MemoryRefStore::new());
    let repo = Repository::open(
        dir,
        &(RepositoryOptions::empty()
            .with_ref_store(store.clone())
            .with_initial_branch("master")),
    )?;
    let tree = repo.odb().write(ObjectKind::Tree, b"")?;
    let commit_body =
        format!("tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\ninit\n");
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())?;
    store
        .prepare(
            RefTransaction::new()
                .update(
                    RefUpdate::new(
                        RefName::from_trusted("refs/heads/master"),
                        Some(RawRef::Direct(commit)),
                    )
                    .with_expected(Expected::Missing),
                )
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");
    Ok((repo, store, commit))
}

#[test]
fn injected_ref_store_v0_advertises_head_with_symref() -> grit_lib::remote::serve::Result<()> {
    let dir = tempfile::tempdir().map_err(ServeError::Io)?;
    let (repo, _store, commit) = memory_serve_repo(dir.path())?;
    let out = grit_v0_advertisement(&repo);
    let text = std::str::from_utf8(&out).expect("utf8");
    assert!(
        text.contains(&format!("{} HEAD", commit.to_hex())),
        "missing HEAD line: {text}"
    );
    assert!(
        text.contains("symref=HEAD:refs/heads/master"),
        "missing symref capability: {text}"
    );
    let names = v0_advertised_ref_names(&out);
    assert!(names.iter().any(|n| n == "HEAD"));
    Ok(())
}

#[test]
fn injected_ref_store_v2_ls_refs_advertises_head_symref() -> grit_lib::remote::serve::Result<()> {
    let dir = tempfile::tempdir().map_err(ServeError::Io)?;
    let (repo, _store, commit) = memory_serve_repo(dir.path())?;
    let mut input = Vec::new();
    input.extend_from_slice(&pkt_line("command=ls-refs"));
    input.extend_from_slice(b"0001");
    input.extend_from_slice(&pkt_line("symrefs"));
    input.extend_from_slice(&pkt_line("ref-prefix HEAD"));
    input.extend_from_slice(&pkt_line("ref-prefix refs/"));
    input.extend_from_slice(b"0000");

    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        stateless_rpc: false,
        advertise_refs: false,
        agent: "grit-test".to_owned(),
        hidden_refs: Vec::new(),
        ..ServeOptions::default()
    };
    let mut out = Vec::new();
    UploadPack::new(&repo, &mut opts).run(&mut Cursor::new(input.as_slice()), &mut out)?;
    let body = String::from_utf8_lossy(&out);
    assert!(
        body.contains(&format!(
            "{} HEAD symref-target:refs/heads/master",
            commit.to_hex()
        )),
        "missing v2 HEAD symref line: {body}"
    );
    Ok(())
}

#[test]
fn v0_advertisement_on_memory_primary_clone_matches_files_clone() {
    let (upstream, history) = crate::support::tagged_upstream();
    let (_files_dir, files_repo, _) = crate::support::clone_upstream(upstream.path(), false);
    let (_memory_dir, memory_repo, _) = crate::support::clone_upstream(upstream.path(), true);
    let files = grit_v0_advertisement(&files_repo);
    let memory = grit_v0_advertisement(&memory_repo);
    assert_eq!(
        String::from_utf8_lossy(&memory),
        String::from_utf8_lossy(&files)
    );
    let text = String::from_utf8_lossy(&memory);
    for (name, peel) in [
        ("nested", history.commit),
        ("treetag", history.tree),
        ("v1", history.commit),
    ] {
        let line = format!("{peel} refs/tags/{name}^{{}}\n");
        assert!(text.contains(&line), "missing {line:?} in {text}");
    }
}
