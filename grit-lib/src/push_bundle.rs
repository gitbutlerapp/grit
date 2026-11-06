//! Resumable pushes: sending a large push as a bundle over TUS.
//!
//! A server whose receive-pack advertises `pushBundle=<path>` accepts uploads
//! at that path (relative to the repository URL) using the [TUS] protocol.
//! When a smart-HTTP push would send a pack larger than
//! [`crate::transfer::PushOptions::resumable_threshold`] (16 MiB unless
//! configured), [`crate::push`] writes the pack as a bundle under
//! `<git-dir>/grit/push-bundles/<session>/`, uploads it, and then asks the
//! server to apply it by posting the usual receive-pack command list to
//! `<upload>/receive`. The answer is a normal receive-pack report.
//!
//! The session is keyed by the remote URL and the exact ref updates, and keeps
//! the bundle and the upload's URL on disk. Running the same push again after
//! an interruption therefore skips building the pack and sends only the bytes
//! the server does not have yet.
//!
//! [TUS]: https://tus.io/protocols/resumable-upload

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fetch::Progress;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::remote::RemoteError;
use crate::resumable::{resolve_url, ResumableError, TusUpload};
use crate::transport::http::{HttpClient, HttpMethod, HttpRequest};

/// Packs above this size use the resumable endpoint when no threshold is set.
pub(crate) const DEFAULT_THRESHOLD: u64 = 16 * 1024 * 1024;

/// Path appended to an upload's URL to apply it.
pub const RECEIVE_SUFFIX: &str = "receive";

/// Resolve the advertised `pushBundle` value against the repository URL.
///
/// A full URL is used as is; anything else is a path under the repository
/// URL, with or without a leading `/` (`/push` on `https://host/repo.git` is
/// `https://host/repo.git/push`).
#[must_use]
pub(crate) fn resolve_endpoint(repo_url: &str, value: &str) -> Option<String> {
    if url::Url::parse(value).is_ok() {
        return Some(value.to_owned());
    }
    let base = format!("{}/", repo_url.trim_end_matches('/'));
    resolve_url(&base, value.trim_start_matches('/'))
}

/// The upload a session made, kept as JSON next to its bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SavedUpload {
    url: String,
    length: u64,
}

/// A push's resumable state on disk.
#[derive(Debug, Clone)]
pub(crate) struct Session {
    dir: PathBuf,
}

impl Session {
    /// The session for pushing `commands` (the receive-pack command block) to
    /// `repo_url` from the repository at `git_dir`.
    pub(crate) fn locate(git_dir: &Path, repo_url: &str, commands: &[u8]) -> Self {
        let mut hasher = crate::hash::ChecksumHasher::new(crate::objects::HashAlgo::Sha256);
        hasher.update(repo_url.as_bytes());
        hasher.update(&[0]);
        hasher.update(commands);
        let key = hex::encode(&hasher.finalize()[..12]);
        Self {
            dir: git_dir.join("grit").join("push-bundles").join(key),
        }
    }

    fn bundle_path(&self) -> PathBuf {
        self.dir.join("push.bundle")
    }

    fn upload_path(&self) -> PathBuf {
        self.dir.join("upload.json")
    }

    /// Whether an earlier run left a finished bundle for this push.
    pub(crate) fn has_bundle(&self) -> bool {
        self.bundle_path().is_file()
    }

    /// Write the bundle: `tips` (new value and destination ref) on top of the
    /// pack `write_pack` streams, which may be thin against `haves`.
    ///
    /// The pack goes straight to disk. When it turns out no larger than
    /// `threshold` bytes it is not worth a resumable upload: no bundle is kept,
    /// and the pack comes back as [`Spooled::Small`] to send the usual way.
    pub(crate) fn write_bundle(
        &self,
        odb: &Odb,
        haves: &[ObjectId],
        tips: &[(ObjectId, String)],
        threshold: u64,
        write_pack: impl FnOnce(&mut dyn Write) -> Result<()>,
    ) -> Result<Spooled> {
        std::fs::create_dir_all(&self.dir)?;
        let mut prerequisites: Vec<(ObjectId, Option<String>)> = Vec::new();
        for have in haves {
            if let Ok(Some(commit)) = odb.peel_to_commit(*have) {
                if !prerequisites.iter().any(|(p, _)| *p == commit) {
                    prerequisites.push((commit, None));
                }
            }
        }
        let mut header = Vec::new();
        crate::bundle::write_bundle_header(&mut header, odb.hash_algo(), &prerequisites, tips)
            .map_err(|e| {
                Error::Remote(crate::remote::RemoteError::Fatal {
                    message: format!("bundle: {e}"),
                })
            })?;
        let mut spool = SpooledPack::create(self.dir.join("push.bundle.tmp"))?;
        let mut out = std::io::BufWriter::new(&spool.file);
        out.write_all(&header)?;
        write_pack(&mut out)?;
        out.flush()?;
        drop(out);
        let header_len = header.len() as u64;
        let pack_len = spool.file.metadata()?.len().saturating_sub(header_len);
        if pack_len <= threshold {
            spool.file.seek(SeekFrom::Start(header_len))?;
            return Ok(Spooled::Small(spool));
        }
        spool.file.sync_all()?;
        std::fs::rename(&spool.path, self.bundle_path())?;
        spool.path.clear();
        let _ = std::fs::remove_file(self.upload_path());
        Ok(Spooled::Bundle)
    }

    fn saved_upload(&self) -> Option<TusUpload> {
        let saved: SavedUpload =
            serde_json::from_slice(&std::fs::read(self.upload_path()).ok()?).ok()?;
        Some(TusUpload {
            url: saved.url,
            length: saved.length,
        })
    }

    fn save_upload(&self, upload: &TusUpload) -> Result<()> {
        let saved = SavedUpload {
            url: upload.url.clone(),
            length: upload.length,
        };
        let bytes = serde_json::to_vec(&saved).map_err(|e| Error::PathError(e.to_string()))?;
        std::fs::write(self.upload_path(), bytes)?;
        Ok(())
    }

    /// Forget the session (after the server applied it).
    pub(crate) fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What [`Session::write_bundle`] left behind.
pub(crate) enum Spooled {
    /// The bundle is on disk, ready to upload.
    Bundle,
    /// The pack was below the threshold; send it over the connection instead.
    Small(SpooledPack),
}

/// A pack written to a scratch file, removed when dropped.
pub(crate) struct SpooledPack {
    file: std::fs::File,
    /// Empty once the file was kept under another name.
    path: PathBuf,
}

impl SpooledPack {
    fn create(path: PathBuf) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self { file, path })
    }

    /// Copy the rest of the pack to `w`.
    ///
    /// # Returns
    ///
    /// The number of bytes copied.
    pub(crate) fn copy_to(mut self, w: &mut dyn Write) -> Result<u64> {
        Ok(std::io::copy(&mut self.file, w)?)
    }
}

impl Drop for SpooledPack {
    fn drop(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
        // Only succeeds when the session holds nothing else.
        if let Some(session_dir) = self.path.parent() {
            let _ = std::fs::remove_dir(session_dir);
        }
    }
}

fn resumable(e: ResumableError) -> RemoteError {
    RemoteError::Resumable(Box::new(e))
}

fn remote_from_err(e: Error) -> RemoteError {
    RemoteError::from(e)
}

/// Upload the session's bundle to `endpoint` (resuming an earlier upload),
/// then post `commands` to apply it.
///
/// # Returns
///
/// The raw receive-pack response (side-band framed when `sideband`).
pub(crate) fn push(
    client: &dyn HttpClient,
    session: &Session,
    endpoint: &str,
    commands: &[u8],
    sent_refs: &[&str],
    sideband: bool,
    progress: &mut dyn Progress,
) -> crate::remote::RemoteResult<Vec<u8>> {
    let bundle = session.bundle_path();
    let length = std::fs::metadata(&bundle)
        .map_err(|e| remote_from_err(Error::Io(e)))?
        .len();
    let mut upload = match session.saved_upload() {
        Some(u) if u.length == length && u.offset(client).map_err(resumable)?.is_some() => u,
        _ => TusUpload::create(client, endpoint, length).map_err(resumable)?,
    };
    session.save_upload(&upload).map_err(remote_from_err)?;
    match upload.send_file(client, &bundle, progress) {
        Ok(()) => {}
        // The server forgot the upload (it expires): start a new one, once.
        Err(ResumableError::Status {
            status: 404 | 410, ..
        }) => {
            upload = TusUpload::create(client, endpoint, length).map_err(resumable)?;
            session.save_upload(&upload).map_err(remote_from_err)?;
            upload
                .send_file(client, &bundle, progress)
                .map_err(resumable)?;
        }
        Err(e) => return Err(resumable(e)),
    }

    let url = format!("{}/{RECEIVE_SUFFIX}", upload.url.trim_end_matches('/'));
    let request = HttpRequest::new(HttpMethod::Post, &url)
        .header("Content-Type", "application/x-git-receive-pack-request")
        .header("Accept", "application/x-git-receive-pack-result")
        .body(commands)
        .long_running();
    let response = client.send(&request)?;
    if !response.is_success() {
        let status = response.status;
        let detail = response
            .into_bytes()
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
            .unwrap_or_default();
        return Err(resumable(ResumableError::Protocol {
            url,
            detail: format!("HTTP {status} applying the pushed bundle: {detail}"),
        }));
    }
    let mut body = response.body;
    let raw = crate::push::read_receive_pack_response(
        &mut body,
        sent_refs,
        sideband,
        &crate::progress::CancelToken::never(),
    )?;
    session.remove();
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_resolve_under_the_repository() {
        assert_eq!(
            resolve_endpoint("http://h:8/repo.git", "/push").as_deref(),
            Some("http://h:8/repo.git/push")
        );
        assert_eq!(
            resolve_endpoint("http://h:8/repo/", "push").as_deref(),
            Some("http://h:8/repo/push")
        );
        assert_eq!(
            resolve_endpoint("http://h/repo", "https://u/x").as_deref(),
            Some("https://u/x")
        );
    }

    use crate::fetch::NoProgress;
    use crate::transport::http::HttpResponse;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A TUS endpoint whose PATCHes can be made to fail, with a `receive` that
    /// answers like receive-pack.
    #[derive(Default)]
    struct Server {
        stored: Mutex<Vec<u8>>,
        length: AtomicUsize,
        uploads: AtomicUsize,
        patched: AtomicUsize,
        failing: AtomicBool,
        applied: Mutex<Option<Vec<u8>>>,
    }

    impl HttpClient for Server {
        fn get(&self, _: &str, _: Option<&str>) -> crate::remote::RemoteResult<Vec<u8>> {
            unreachable!()
        }
        fn post(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: Option<&str>,
        ) -> crate::remote::RemoteResult<Vec<u8>> {
            unreachable!()
        }
        fn send(&self, request: &HttpRequest<'_>) -> crate::remote::RemoteResult<HttpResponse> {
            let header = |name: &str| {
                request
                    .headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .and_then(|(_, v)| v.parse::<usize>().ok())
            };
            let reply = |status: u16, headers: Vec<(String, String)>, body: Vec<u8>| HttpResponse {
                status,
                headers,
                body: Box::new(std::io::Cursor::new(body)),
            };
            let mut stored = self.stored.lock().unwrap_or_else(|e| e.into_inner());
            let offset = |n: usize| vec![("upload-offset".to_owned(), n.to_string())];
            Ok(match (request.method, request.url) {
                (HttpMethod::Post, "http://h/r/push") => {
                    self.uploads.fetch_add(1, Ordering::SeqCst);
                    stored.clear();
                    self.length
                        .store(header("Upload-Length").unwrap_or(0), Ordering::SeqCst);
                    reply(
                        201,
                        vec![("location".to_owned(), "/r/push/u".to_owned())],
                        vec![],
                    )
                }
                (HttpMethod::Head, _) => reply(200, offset(stored.len()), vec![]),
                (HttpMethod::Patch, _) => {
                    if self.failing.load(Ordering::SeqCst) && !stored.is_empty() {
                        return Err(crate::remote::RemoteError::Transport {
                            url: None,
                            detail: "connection reset".to_owned(),
                        });
                    }
                    if header("Upload-Offset") != Some(stored.len()) {
                        return Ok(reply(409, vec![], vec![]));
                    }
                    self.patched.fetch_add(request.body.len(), Ordering::SeqCst);
                    stored.extend_from_slice(request.body);
                    reply(204, offset(stored.len()), vec![])
                }
                (HttpMethod::Post, "http://h/r/push/u/receive") => {
                    assert_eq!(stored.len(), self.length.load(Ordering::SeqCst));
                    *self.applied.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(request.body.to_vec());
                    let mut report = Vec::new();
                    crate::pkt_line::write_line_to_vec(&mut report, "unpack ok")
                        .map_err(crate::remote::RemoteError::from_io)?;
                    crate::pkt_line::write_line_to_vec(&mut report, "ok refs/heads/main")
                        .map_err(crate::remote::RemoteError::from_io)?;
                    report.extend_from_slice(b"0000");
                    reply(200, vec![], report)
                }
                _ => reply(404, vec![], vec![]),
            })
        }
    }

    #[test]
    fn an_interrupted_push_resumes_where_the_server_stopped() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("tempdir: {e}"),
        };
        let session = Session::locate(dir.path(), "http://h/r", b"commands");
        assert!(std::fs::create_dir_all(&session.dir).is_ok());
        let data: Vec<u8> = (0..9_000_000u32).map(|i| (i % 249) as u8).collect();
        assert!(std::fs::write(session.bundle_path(), &data).is_ok());
        assert!(session.has_bundle());

        let server = Server::default();
        server.failing.store(true, Ordering::SeqCst);
        let first = push(
            &server,
            &session,
            "http://h/r/push",
            b"commands",
            &["refs/heads/main"],
            false,
            &mut NoProgress,
        );
        assert!(first.is_err(), "the connection kept dropping");
        let kept = server
            .stored
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert!(kept > 0 && kept < data.len());
        assert!(session.has_bundle() && session.saved_upload().is_some());

        server.failing.store(false, Ordering::SeqCst);
        let report = push(
            &server,
            &session,
            "http://h/r/push",
            b"commands",
            &["refs/heads/main"],
            false,
            &mut NoProgress,
        );
        let report = match report {
            Ok(r) => String::from_utf8_lossy(&r).into_owned(),
            Err(e) => panic!("resume: {e}"),
        };
        assert!(report.contains("ok refs/heads/main"), "{report}");
        assert_eq!(
            server.uploads.load(Ordering::SeqCst),
            1,
            "the upload was reused"
        );
        assert_eq!(
            server.patched.load(Ordering::SeqCst),
            data.len(),
            "no byte was sent twice"
        );
        assert_eq!(
            *server.stored.lock().unwrap_or_else(|e| e.into_inner()),
            data
        );
        assert_eq!(
            server
                .applied
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref(),
            Some(&b"commands"[..])
        );
        assert!(!session.dir.exists(), "the session is removed once applied");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_large_pack_streams_into_a_bundle_git_can_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = grit_test_support::git_packed_history(tmp.path(), 40);
        let tip = ObjectId::from_hex(&git(&bare, &["rev-parse", "main"])).unwrap();
        let odb = Odb::new(&bare.join("objects"));
        let session = Session::locate(&bare, "http://h/r", b"cmds");
        let opts = crate::pack_objects::PackBuildOptions::for_local_copy(None);
        let mut streamed = 0u64;
        let spooled = session
            .write_bundle(&odb, &[], &[(tip, "refs/heads/main".to_owned())], 0, |w| {
                streamed = crate::pack_objects::write_pack_for_push(&odb, &[tip], &[], &opts, w)?
                    .pack_bytes;
                Ok(())
            })
            .unwrap();
        assert!(matches!(spooled, Spooled::Bundle));
        assert!(session.has_bundle());
        assert!(!session.dir.join("push.bundle.tmp").exists());
        let bundle_len = std::fs::metadata(session.bundle_path()).unwrap().len();
        assert!(streamed > 0 && bundle_len > streamed);

        let bundle = session.bundle_path();
        let bundle = bundle.to_str().unwrap();
        git(&bare, &["bundle", "verify", bundle]);
        git(tmp.path(), &["clone", "-q", "--bare", bundle, "copy.git"]);
        let copy = tmp.path().join("copy.git");
        git(&copy, &["fsck", "--strict", "--no-dangling"]);
        assert_eq!(git(&copy, &["rev-parse", "main"]), tip.to_hex());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_pack_under_the_threshold_comes_back_and_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = grit_test_support::git_packed_history(tmp.path(), 3);
        let tip = ObjectId::from_hex(&git(&bare, &["rev-parse", "main"])).unwrap();
        let odb = Odb::new(&bare.join("objects"));
        let session = Session::locate(&bare, "http://h/r", b"cmds");
        let spooled = session
            .write_bundle(
                &odb,
                &[],
                &[(tip, "refs/heads/main".to_owned())],
                1024,
                |w| Ok(w.write_all(b"PACK and the rest")?),
            )
            .unwrap();
        let Spooled::Small(small) = spooled else {
            panic!("a 17-byte pack is under a 1 KiB threshold");
        };
        assert!(!session.has_bundle());
        let mut sent = Vec::new();
        assert_eq!(small.copy_to(&mut sent).unwrap(), 17);
        assert_eq!(sent, b"PACK and the rest");
        assert!(
            !session.dir.exists(),
            "the spooled pack and its session are removed"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_failed_pack_write_leaves_no_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = grit_test_support::git_packed_history(tmp.path(), 3);
        let tip = ObjectId::from_hex(&git(&bare, &["rev-parse", "main"])).unwrap();
        let odb = Odb::new(&bare.join("objects"));
        let session = Session::locate(&bare, "http://h/r", b"cmds");
        let result =
            session.write_bundle(&odb, &[], &[(tip, "refs/heads/main".to_owned())], 0, |w| {
                w.write_all(b"PACK")?;
                Err(Error::ObjectNotFound("object went missing".to_owned()))
            });
        assert!(result.is_err());
        assert!(!session.has_bundle());
        assert!(!session.dir.exists());
    }

    #[test]
    fn sessions_are_keyed_by_remote_and_updates() {
        let dir = Path::new("/r/.git");
        let a = Session::locate(dir, "http://h/r", b"cmds");
        assert_eq!(a.dir, Session::locate(dir, "http://h/r", b"cmds").dir);
        assert_ne!(a.dir, Session::locate(dir, "http://h/s", b"cmds").dir);
        assert_ne!(a.dir, Session::locate(dir, "http://h/r", b"other").dir);
        assert!(a.dir.starts_with("/r/.git/grit/push-bundles"));
    }
}
