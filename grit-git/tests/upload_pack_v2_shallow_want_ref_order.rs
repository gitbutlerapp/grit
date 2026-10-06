//! Regression: v2 upload-pack must emit `shallow-info` before `wanted-refs` when the
//! client sends `want-ref` together with a deepen request (upstream 2fd3d524).

use grit_lib::objects::ObjectId;
use grit_lib::pkt_line::{self, Packet};
use grit_lib::protocol_v2::{fetch_prelude_shallow_before_wanted_refs, FetchResponseSection};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn init_repo_with_linear_history(grit: &str, repo: &std::path::Path) -> ObjectId {
    assert!(Command::new(grit)
        .args(["init", "-b", "main", repo.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let mut tip = ObjectId::from_hex("0".repeat(40).as_str()).unwrap();
    for msg in ["a", "b", "c"] {
        let file = repo.join(format!("{msg}.txt"));
        std::fs::write(&file, msg).unwrap();
        assert!(Command::new(grit)
            .args([
                "-C",
                repo.to_str().unwrap(),
                "add",
                file.file_name().unwrap().to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new(grit)
            .args([
                "-C",
                repo.to_str().unwrap(),
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                msg,
            ])
            .status()
            .unwrap()
            .success());
        let out = Command::new(grit)
            .args(["-C", repo.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap();
        tip =
            ObjectId::from_hex(std::str::from_utf8(out.stdout.as_slice()).unwrap().trim()).unwrap();
    }
    assert!(Command::new(grit)
        .args([
            "-C",
            repo.to_str().unwrap(),
            "config",
            "uploadpack.allowRefInWant",
            "true",
        ])
        .status()
        .unwrap()
        .success());
    tip
}

fn write_v2_fetch_want_ref_deepen(
    w: &mut impl Write,
    want_ref: &str,
    shallow_boundary: ObjectId,
    depth: u32,
) -> std::io::Result<()> {
    pkt_line::write_line(w, "command=fetch")?;
    pkt_line::write_line(w, "agent=grit-test")?;
    pkt_line::write_line(w, "object-format=sha1")?;
    pkt_line::write_delim(w)?;
    pkt_line::write_line(w, "thin-pack")?;
    pkt_line::write_line(w, "no-progress")?;
    pkt_line::write_line(w, "ofs-delta")?;
    pkt_line::write_line(w, &format!("want-ref {want_ref}"))?;
    pkt_line::write_line(w, &format!("shallow {}", shallow_boundary.to_hex()))?;
    pkt_line::write_line(w, &format!("deepen {depth}"))?;
    pkt_line::write_line(w, "done")?;
    pkt_line::write_flush(w)?;
    w.flush()
}

fn collect_fetch_prelude_section_headers(r: &mut impl Read) -> Vec<String> {
    let mut headers = Vec::new();
    loop {
        match pkt_line::read_packet(r).expect("read v2 response packet") {
            None | Some(Packet::Flush) => break,
            Some(Packet::Delim) => continue,
            Some(Packet::ResponseEnd) => break,
            Some(Packet::Data(line)) => {
                let hdr = line.trim_end().to_string();
                if hdr == FetchResponseSection::Packfile.header() {
                    break;
                }
                if hdr == FetchResponseSection::ShallowInfo.header()
                    || hdr == FetchResponseSection::WantedRefs.header()
                    || hdr == FetchResponseSection::Acknowledgments.header()
                    || hdr == FetchResponseSection::PackfileUris.header()
                {
                    headers.push(hdr.clone());
                    skip_v2_section(r);
                }
            }
        }
    }
    headers
}

fn skip_v2_section(r: &mut impl Read) {
    loop {
        match pkt_line::read_packet(r).expect("skip section") {
            None | Some(Packet::Flush) | Some(Packet::Delim) | Some(Packet::ResponseEnd) => break,
            Some(Packet::Data(_)) => {}
        }
    }
}

#[test]
fn upload_pack_v2_shallow_info_precedes_wanted_refs_for_want_ref_deepen() {
    let grit = env!("CARGO_BIN_EXE_grit-git");
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    let tip = init_repo_with_linear_history(grit, &repo);

    // Parent of tip (depth-1 boundary for a shallow clone of `main`).
    let parent_out = Command::new(grit)
        .args([
            "-C",
            repo.to_str().unwrap(),
            "rev-parse",
            &format!("{tip}^"),
        ])
        .output()
        .unwrap();
    let shallow_boundary = ObjectId::from_hex(
        std::str::from_utf8(parent_out.stdout.as_slice())
            .unwrap()
            .trim(),
    )
    .unwrap();

    let mut child = Command::new(grit)
        .arg("upload-pack")
        .arg(&repo)
        .env("GIT_PROTOCOL", "version=2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = child.stdout.take().unwrap();
    let mut stdin = child.stdin.take().unwrap();

    // Stateful v2: server advertises capabilities first (flush-terminated).
    loop {
        match pkt_line::read_packet(&mut stdout).expect("read caps") {
            None => panic!("EOF in capability advertisement"),
            Some(Packet::Flush) => break,
            Some(Packet::Data(_)) => {}
            Some(other) => panic!("unexpected cap packet: {other:?}"),
        }
    }

    write_v2_fetch_want_ref_deepen(&mut stdin, "refs/heads/main", shallow_boundary, 2).unwrap();
    drop(stdin);

    let headers = collect_fetch_prelude_section_headers(&mut stdout);
    let stderr = child.stderr.take();
    let status = child.wait().unwrap();
    let err_text = stderr
        .map(|mut s| {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut s, &mut buf);
            buf
        })
        .unwrap_or_default();
    assert!(status.success(), "stderr={err_text}");

    assert!(
        headers
            .iter()
            .any(|h| h == FetchResponseSection::ShallowInfo.header()),
        "expected shallow-info section, got {headers:?}"
    );
    assert!(
        headers
            .iter()
            .any(|h| h == FetchResponseSection::WantedRefs.header()),
        "expected wanted-refs section for want-ref deepen, got {headers:?}"
    );
    assert!(
        fetch_prelude_shallow_before_wanted_refs(
            &headers.iter().map(String::as_str).collect::<Vec<_>>()
        ),
        "section order must be shallow-info before wanted-refs, got {headers:?}"
    );
}
