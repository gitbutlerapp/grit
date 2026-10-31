//! Explicit `refs/tags/...` fetch refspecs are honoured under the default tag
//! following, for annotated tags and for tags moved on the remote (issue #2157),
//! over the local transport and over SSH against system `git upload-pack`.

use grit_test_support::rev_parse;
use std::path::{Path, PathBuf};

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::transfer::{fetch_local, FetchOptions, FetchOutcome, TagMode, UpdateMode};
use grit_lib::remote::transport::{ConnectOptions, Service, SshTransport, Transport};
use grit_test_support::git;

fn fsck(dir: &Path) {
    git(dir, &["fsck", "--strict", "--no-dangling"]);
}

/// A repository with one commit, a lightweight tag `v1` and an annotated tag `rel`.
fn upstream(tmp: &Path) -> PathBuf {
    let up = tmp.join("up");
    std::fs::create_dir_all(&up).unwrap();
    git(&up, &["init", "-q", "-b", "main", "."]);
    std::fs::write(up.join("f"), b"1\n").unwrap();
    git(&up, &["add", "f"]);
    git(&up, &["commit", "-q", "-m", "c1"]);
    git(&up, &["tag", "v1"]);
    git(&up, &["tag", "-a", "rel", "-m", "rel"]);
    up
}

/// Retarget the annotated tag `rel` at a new commit.
fn move_rel(up: &Path) {
    std::fs::write(up.join("f"), b"2\n").unwrap();
    git(up, &["commit", "-q", "-am", "c2"]);
    git(up, &["tag", "-f", "-a", "rel", "-m", "moved"]);
}

fn empty_repo(tmp: &Path, name: &str) -> PathBuf {
    let dir = tmp.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main", "."]);
    dir
}

fn grit_fetch(local: &Path, up: &Path, spec: &str) -> FetchOutcome {
    let opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec![spec.to_owned()];
        o.tags = TagMode::Following;
        o
    };
    fetch_local(&local.join(".git"), &up.join(".git"), &opts).expect("grit fetch")
}

fn git_fetch(local: &Path, up: &Path, spec: &str) {
    git(local, &["fetch", "-q", up.to_str().unwrap(), spec]);
}

fn update_mode(outcome: &FetchOutcome, local_ref: &str) -> UpdateMode {
    outcome
        .updates
        .iter()
        .find(|u| u.local_ref.as_deref() == Some(local_ref))
        .unwrap_or_else(|| panic!("{local_ref} missing from {:?}", outcome.updates))
        .mode
}

fn assert_annotated_tag(dir: &Path, name: &str, expected: ObjectId) {
    let got = resolve_ref(&dir.join(".git"), name).expect("tag ref");
    assert_eq!(got, expected, "{name} must point at the tag object");
    assert_eq!(git(dir, &["cat-file", "-t", name]).trim(), "tag");
}

#[test]
fn explicit_annotated_tag_refspec_creates_destination_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let up = upstream(tmp.path());
    let rel = rev_parse(&up, "refs/tags/rel");
    let grit_dn = empty_repo(tmp.path(), "grit-dn");
    let git_dn = empty_repo(tmp.path(), "git-dn");

    for spec in [
        "+refs/tags/rel:refs/tags/rel",
        "refs/tags/rel:refs/tags/rel2",
    ] {
        git_fetch(&git_dn, &up, spec);
        let outcome = grit_fetch(&grit_dn, &up, spec);
        let dst = spec.rsplit(':').next().unwrap();
        assert_eq!(update_mode(&outcome, dst), UpdateMode::New);
        assert_annotated_tag(&grit_dn, dst, rel);
        assert_eq!(rev_parse(&git_dn, dst), rel);
    }
    fsck(&grit_dn);
}

#[test]
fn forced_annotated_tag_refspec_moves_retargeted_tag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let up = upstream(tmp.path());
    let grit_dn = empty_repo(tmp.path(), "grit-dn");
    let git_dn = empty_repo(tmp.path(), "git-dn");
    let spec = "refs/tags/rel:refs/tags/rel";
    grit_fetch(&grit_dn, &up, spec);
    git_fetch(&git_dn, &up, spec);
    let old = rev_parse(&up, "refs/tags/rel");
    move_rel(&up);
    let new = rev_parse(&up, "refs/tags/rel");

    let outcome = grit_fetch(&grit_dn, &up, spec);
    assert_eq!(
        update_mode(&outcome, "refs/tags/rel"),
        UpdateMode::TagUpdateRejected
    );
    assert_eq!(
        resolve_ref(&grit_dn.join(".git"), "refs/tags/rel").unwrap(),
        old
    );

    let forced = format!("+{spec}");
    let outcome = grit_fetch(&grit_dn, &up, &forced);
    assert_eq!(update_mode(&outcome, "refs/tags/rel"), UpdateMode::Forced);
    assert_annotated_tag(&grit_dn, "refs/tags/rel", new);
    git_fetch(&git_dn, &up, &forced);
    assert_eq!(rev_parse(&git_dn, "refs/tags/rel"), new);
    fsck(&grit_dn);
}

#[test]
fn tag_pattern_refspec_is_not_subject_to_following() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let up = upstream(tmp.path());
    let grit_dn = empty_repo(tmp.path(), "grit-dn");

    let outcome = grit_fetch(&grit_dn, &up, "refs/tags/*:refs/tags/*");
    assert_eq!(update_mode(&outcome, "refs/tags/v1"), UpdateMode::New);
    assert_eq!(update_mode(&outcome, "refs/tags/rel"), UpdateMode::New);
    assert_annotated_tag(&grit_dn, "refs/tags/rel", rev_parse(&up, "refs/tags/rel"));
    assert_eq!(
        resolve_ref(&grit_dn.join(".git"), "refs/tags/v1").unwrap(),
        rev_parse(&up, "refs/tags/v1")
    );
    fsck(&grit_dn);
}

#[cfg(unix)]
fn write_fake_ssh(tmp: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = tmp.join("fake-ssh.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while [ "$1" = "-p" ]; do shift 2; done
shift
cmd=$*
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
esac
eval "$cmd"
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

#[cfg(unix)]
fn ssh_fetch(
    fake_ssh: &Path,
    up: &Path,
    local: &Path,
    protocol_v2: bool,
    spec: &str,
) -> FetchOutcome {
    let (transport, url, protocol_version) = if protocol_v2 {
        (
            SshTransport::with_program(fake_ssh.as_os_str()),
            format!("ssh://git@fakehost{}", up.display()),
            2,
        )
    } else {
        (
            SshTransport::with_shell_command(fake_ssh.as_os_str()),
            format!("fakehost:{}", up.display()),
            0,
        )
    };
    let connect_opts = ConnectOptions {
        protocol_version,
        ..Default::default()
    };
    let mut conn = transport
        .connect(&url, Service::UploadPack, &connect_opts)
        .expect("connect");
    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec![spec.to_owned()];
        o.tags = TagMode::Following;
        o
    };
    fetch_remote(
        &local.join(".git"),
        conn.as_mut(),
        &mut opts,
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("ssh fetch")
}

#[cfg(unix)]
#[test]
fn ssh_annotated_tag_refspecs_against_system_upload_pack() {
    for protocol_v2 in [false, true] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fake_ssh = write_fake_ssh(tmp.path());
        let up = upstream(tmp.path());
        let dn = empty_repo(tmp.path(), "dn");

        let outcome = ssh_fetch(
            &fake_ssh,
            &up,
            &dn,
            protocol_v2,
            "refs/tags/rel:refs/tags/rel2",
        );
        assert_eq!(update_mode(&outcome, "refs/tags/rel2"), UpdateMode::New);
        assert_annotated_tag(&dn, "refs/tags/rel2", rev_parse(&up, "refs/tags/rel"));

        ssh_fetch(
            &fake_ssh,
            &up,
            &dn,
            protocol_v2,
            "refs/tags/rel:refs/tags/rel",
        );
        move_rel(&up);
        let outcome = ssh_fetch(
            &fake_ssh,
            &up,
            &dn,
            protocol_v2,
            "+refs/tags/rel:refs/tags/rel",
        );
        assert_eq!(update_mode(&outcome, "refs/tags/rel"), UpdateMode::Forced);
        assert_annotated_tag(&dn, "refs/tags/rel", rev_parse(&up, "refs/tags/rel"));
        fsck(&dn);
    }
}
