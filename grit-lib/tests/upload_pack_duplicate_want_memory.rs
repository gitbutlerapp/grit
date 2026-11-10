//! Issue #2390: duplicate `want` lines must not allocate one entry per line.

#![cfg(all(target_os = "linux", feature = "full-tests"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::serve::{ProtocolVersion, ServeOptions, UploadPack};
use grit_lib::repo::{InitOptions, Repository};

fn anon_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("RssAnon")
}

fn reset_peak_rss() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

const DUPLICATE_WANTS: usize = 2_000_000;
/// Pre-fix cost was about 33 bytes of anon RSS per duplicate want line.
const MAX_GROWTH_KIB: u64 = 8 * 1024;

fn pkt_line(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    format!("{:04x}{}", bytes.len() + 4, payload).into_bytes()
}

fn seed_bare_tip(repo: &Repository) -> ObjectId {
    let blob = repo
        .odb()
        .write_loose_materialize(grit_lib::objects::ObjectKind::Blob, b"x\n")
        .expect("blob");
    let tree_body = format!("100644 f\0").into_bytes();
    let mut tree_bytes = tree_body;
    tree_bytes.extend_from_slice(blob.as_bytes());
    let tree = repo
        .odb()
        .write_loose_materialize(grit_lib::objects::ObjectKind::Tree, &tree_bytes)
        .expect("tree");
    let commit_body = format!(
        "tree {tree}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nm\n"
    );
    let tip = repo
        .odb()
        .write_loose_materialize(
            grit_lib::objects::ObjectKind::Commit,
            commit_body.as_bytes(),
        )
        .expect("commit");
    std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
    tip
}

/// Streams `count` identical want pkt-lines without materializing the full body.
struct StreamingDuplicateWants {
    want_pkt: Vec<u8>,
    pkt_offset: usize,
    remaining: usize,
    tail: Vec<u8>,
    tail_offset: usize,
}

impl StreamingDuplicateWants {
    fn new(want_pkt: Vec<u8>, count: usize, tail: Vec<u8>) -> Self {
        Self {
            want_pkt,
            pkt_offset: 0,
            remaining: count,
            tail,
            tail_offset: 0,
        }
    }

    fn v0(tip: ObjectId, count: usize) -> Self {
        let mut tail = Vec::from(b"0000" as &[u8]);
        tail.extend(pkt_line("done"));
        tail.extend(b"0000");
        Self::new(
            pkt_line(&format!("want {} ofs-delta\n", tip.to_hex())),
            count,
            tail,
        )
    }

    fn v2_wants(tip: ObjectId, count: usize) -> Self {
        let mut tail = Vec::from(b"0000" as &[u8]);
        tail.extend(pkt_line("done"));
        tail.extend(b"0000");
        Self::new(pkt_line(&format!("want {}\n", tip.to_hex())), count, tail)
    }
}

impl Read for StreamingDuplicateWants {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut written = 0usize;
        while written < buf.len() {
            if self.remaining > 0 {
                if self.pkt_offset >= self.want_pkt.len() {
                    self.pkt_offset = 0;
                    self.remaining -= 1;
                    continue;
                }
                let n = (self.want_pkt.len() - self.pkt_offset).min(buf.len() - written);
                buf[written..written + n]
                    .copy_from_slice(&self.want_pkt[self.pkt_offset..self.pkt_offset + n]);
                self.pkt_offset += n;
                written += n;
                continue;
            }
            if self.tail_offset < self.tail.len() {
                let n = (self.tail.len() - self.tail_offset).min(buf.len() - written);
                buf[written..written + n]
                    .copy_from_slice(&self.tail[self.tail_offset..self.tail_offset + n]);
                self.tail_offset += n;
                written += n;
                continue;
            }
            break;
        }
        Ok(written)
    }
}

fn v2_fetch_prefix() -> Vec<u8> {
    let mut body = pkt_line("command=fetch\n");
    body.extend_from_slice(&pkt_line("object-format=sha1\n"));
    body.extend(b"0001");
    body
}

struct ChainRead<P: Read, S: Read> {
    prefix: P,
    stream: S,
    prefix_done: bool,
}

impl<P: Read, S: Read> Read for ChainRead<P, S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.prefix_done {
            let n = self.prefix.read(buf)?;
            if n == 0 {
                self.prefix_done = true;
            } else {
                return Ok(n);
            }
        }
        self.stream.read(buf)
    }
}

fn serve_stream(repo: &Repository, protocol: ProtocolVersion, mut input: impl Read) {
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol,
        stateless_rpc: true,
        ..ServeOptions::default()
    };
    UploadPack::new(repo, &mut opts)
        .run(&mut input, &mut out)
        .expect("upload-pack");
}

#[test]
fn upload_pack_v0_duplicate_wants_peak_memory() {
    if !reset_peak_rss() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let tip = seed_bare_tip(&repo);

    reset_peak_rss();
    let before = anon_rss_kib();
    serve_stream(
        &repo,
        ProtocolVersion::V0,
        StreamingDuplicateWants::v0(tip, DUPLICATE_WANTS),
    );
    let growth = anon_rss_kib().saturating_sub(before);
    assert!(
        growth < MAX_GROWTH_KIB,
        "v0 duplicate wants grew anon RSS by {growth} KiB (limit {MAX_GROWTH_KIB} KiB)"
    );
}

#[test]
fn upload_pack_v2_duplicate_wants_peak_memory() {
    if !reset_peak_rss() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let tip = seed_bare_tip(&repo);

    reset_peak_rss();
    let before = anon_rss_kib();
    serve_stream(
        &repo,
        ProtocolVersion::V2,
        ChainRead {
            prefix: std::io::Cursor::new(v2_fetch_prefix()),
            stream: StreamingDuplicateWants::v2_wants(tip, DUPLICATE_WANTS),
            prefix_done: false,
        },
    );
    let growth = anon_rss_kib().saturating_sub(before);
    assert!(
        growth < MAX_GROWTH_KIB,
        "v2 duplicate wants grew anon RSS by {growth} KiB (limit {MAX_GROWTH_KIB} KiB)"
    );
}
