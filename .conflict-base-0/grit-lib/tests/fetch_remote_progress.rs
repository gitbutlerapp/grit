//! Remote side-band channel-2 progress against system `git upload-pack`.
//!
//! Asserts `FetchOptions::remote_progress` omits or includes `no-progress` on the
//! wire and controls whether band-2 bytes arrive, for protocol v0/v1 and v2.

#![cfg(unix)]

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

use grit_lib::objects::ObjectId;
use grit_lib::progress::ProgressSink;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::{fetch_remote, Progress};
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::remote::transport::{ConnectOptions, Connection, Service, SshTransport, Transport};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn build_progress_source(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..32 {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("payload {i}\n")).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
}

fn write_fake_ssh(tmp: &Path) -> Option<std::path::PathBuf> {
    let script = tmp.join("fake-ssh.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while [ "$1" = "-p" ]; do shift 2; done
host=$1
shift
cmd=$*
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
esac
eval "$cmd"
"#,
    )
    .ok()?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

struct TeeWriter {
    inner: *mut dyn Connection,
    captured: Rc<RefCell<Vec<u8>>>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: `inner` points at the `Box<dyn Connection>` owned by `TeeConn`.
        let conn: &mut dyn Connection = unsafe { &mut *self.inner };
        self.captured.borrow_mut().extend_from_slice(buf);
        conn.writer().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let conn: &mut dyn Connection = unsafe { &mut *self.inner };
        conn.writer().flush()
    }
}

struct TeeConn {
    inner: Box<dyn Connection>,
    request_bytes: Rc<RefCell<Vec<u8>>>,
    tee_writer_slot: Option<TeeWriter>,
}

impl TeeConn {
    fn new(inner: Box<dyn Connection>) -> Self {
        Self {
            inner,
            request_bytes: Rc::new(RefCell::new(Vec::new())),
            tee_writer_slot: None,
        }
    }

    fn request_bytes(&self) -> Rc<RefCell<Vec<u8>>> {
        Rc::clone(&self.request_bytes)
    }
}

impl Connection for TeeConn {
    fn reader(&mut self) -> &mut dyn Read {
        self.inner.reader()
    }

    fn writer(&mut self) -> &mut dyn Write {
        let ptr: *mut dyn Connection = self.inner.as_mut();
        self.tee_writer_slot = Some(TeeWriter {
            inner: ptr,
            captured: Rc::clone(&self.request_bytes),
        });
        self.tee_writer_slot.as_mut().unwrap()
    }

    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        self.inner.advertised_refs()
    }

    fn capabilities(&self) -> &[String] {
        self.inner.capabilities()
    }

    fn head_symref(&self) -> Option<&str> {
        self.inner.head_symref()
    }

    fn protocol_version(&self) -> u8 {
        self.inner.protocol_version()
    }

    fn finish_send(&mut self) {
        self.inner.finish_send();
    }
}

struct RemoteCap(Vec<u8>);

impl ProgressSink for RemoteCap {}

impl Progress for RemoteCap {
    fn message(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn remote_sideband_progress(&self) -> bool {
        true
    }
}

fn run_case(protocol_v2: bool, remote_progress: bool) -> (Vec<u8>, Vec<u8>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    build_progress_source(&source);

    let fake_ssh = write_fake_ssh(tmp.path()).expect("fake ssh");
    let transport = if protocol_v2 {
        SshTransport::with_program(fake_ssh.as_os_str())
    } else {
        SshTransport::with_shell_command(fake_ssh.as_os_str())
    };
    let abs_path = source.to_str().expect("utf8");
    let url = if protocol_v2 {
        format!("ssh://git@fakehost{abs_path}")
    } else {
        format!("fakehost:{abs_path}")
    };

    let local_root = tmp.path().join("local");
    std::fs::create_dir_all(&local_root).unwrap();
    git(&local_root, &["init", "-q", "-b", "main", "."]);
    let local_git = local_root.join(".git");

    let connect_opts = if protocol_v2 {
        ConnectOptions {
            protocol_version: 2,
            ..Default::default()
        }
    } else {
        ConnectOptions::default()
    };

    let inner = transport
        .connect(&url, Service::UploadPack, &connect_opts)
        .expect("connect");
    if protocol_v2 && inner.protocol_version() != 2 {
        panic!("SKIP: upload-pack did not negotiate v2");
    }
    let mut conn = TeeConn::new(inner);
    let req_capture = conn.request_bytes();

    let main_oid = rev_parse(&source, "refs/heads/main");
    let mut cap = RemoteCap(Vec::new());
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        remote_progress,
        ..Default::default()
    };
    fetch_remote(&local_git, &mut conn, &opts, &mut cap).expect("fetch");

    let got = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    assert_eq!(got, main_oid);

    let fsck = Command::new("git")
        .current_dir(&local_root)
        .args(["fsck", "--strict"])
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    let req = req_capture.borrow().clone();
    (req, cap.0)
}

fn request_has_no_progress(req: &[u8]) -> bool {
    String::from_utf8_lossy(req).contains("no-progress")
}

#[test]
fn fetch_remote_progress_receives_channel2_from_system_git() {
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        eprintln!("SKIP: no POSIX sh");
        return;
    }

    for protocol_v2 in [false, true] {
        let (req_on, band_on) = run_case(protocol_v2, true);
        assert!(
            !request_has_no_progress(&req_on),
            "v{}: remote_progress=true must omit no-progress on wire",
            if protocol_v2 { 2 } else { 0 }
        );
        assert!(
            !band_on.is_empty(),
            "v{}: expected non-empty channel-2 with remote_progress",
            if protocol_v2 { 2 } else { 0 }
        );

        let (req_off, band_off) = run_case(protocol_v2, false);
        assert!(
            request_has_no_progress(&req_off),
            "v{}: remote_progress=false must send no-progress",
            if protocol_v2 { 2 } else { 0 }
        );
        assert!(
            band_off.is_empty(),
            "v{}: expected no channel-2 with no-progress",
            if protocol_v2 { 2 } else { 0 }
        );
    }
}
