//! Pre-generated v0 stateless-RPC upload-pack request bodies for benchmarks.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

/// Capabilities on the first `want` line for clone-style serve benchmarks.
pub const SERVE_CLONE_CAPABILITIES: &[&str] = &["side-band-64k", "thin-pack", "ofs-delta"];

/// Relative path under a prepared bitmap fixture where the request bytes are stored.
pub const SERVE_CLONE_REQUEST_FILE: &str = ".grit-bench-serve-clone-req.bin";

/// Build a v0 stateless upload-pack request that names every advertised ref tip.
///
/// The wire format matches what `git upload-pack --stateless-rpc` expects: one
/// `want` pkt-line per ref (capabilities on the first line), flush, `done`, flush.
pub fn write_v0_serve_clone_request(repo: &Repository, out: &mut impl Write) -> Result<()> {
    let mut tips = ref_want_oids(repo)?;
    if tips.is_empty() {
        tips.push(ObjectId::null(repo.odb.hash_algo()));
    }
    tips.sort_by_key(|oid| oid.to_hex());
    tips.dedup();

    let mut buf = Vec::new();
    let caps = serve_clone_capability_string(repo);
    for (i, oid) in tips.iter().enumerate() {
        let line = if i == 0 {
            format!("want {} {caps}", oid.to_hex())
        } else {
            format!("want {}", oid.to_hex())
        };
        grit_lib::pkt_line::write_line_to_vec(&mut buf, &line).context("write want line")?;
    }
    buf.extend_from_slice(grit_lib::pkt_line::FLUSH.as_bytes());
    grit_lib::pkt_line::write_line_to_vec(&mut buf, "done").context("write done")?;
    buf.extend_from_slice(grit_lib::pkt_line::FLUSH.as_bytes());
    out.write_all(&buf).context("write serve-clone request")?;
    Ok(())
}

fn serve_clone_capability_string(repo: &Repository) -> String {
    let mut caps: Vec<String> = SERVE_CLONE_CAPABILITIES
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    caps.push(format!("object-format={}", repo.odb.hash_algo().name()));
    caps.join(" ")
}

/// Collect ref tips in the same order Git uses for a full-clone want list.
fn ref_want_oids(repo: &Repository) -> Result<Vec<ObjectId>> {
    let mut refs = grit_lib::refs::list_refs(&repo.git_dir, "refs/")?;
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (name, oid) in refs {
        if !grit_lib::refs::is_valid_fetch_advertised_ref(&name) {
            continue;
        }
        if seen.insert(oid) {
            out.push(oid);
        }
    }
    Ok(out)
}

/// Write the cached serve-clone request next to `repo` if missing.
pub fn ensure_serve_clone_request(repo: &Path) -> Result<std::path::PathBuf> {
    let path = repo.join(SERVE_CLONE_REQUEST_FILE);
    if path.is_file() {
        return Ok(path);
    }
    let repository = Repository::open(repo, None).context("open repo for serve request")?;
    let mut file = std::fs::File::create(&path).context("create serve-clone request file")?;
    write_v0_serve_clone_request(&repository, &mut file)?;
    Ok(path)
}

/// Parse pkt-lines from a request body (for tests and validation).
pub fn parse_v0_request_lines(body: &[u8]) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut i = 0usize;
    while i + 4 <= body.len() {
        let len_str = std::str::from_utf8(&body[i..i + 4]).context("pkt length")?;
        if len_str == "0000" {
            lines.push(String::new());
            i += 4;
            continue;
        }
        let len = usize::from_str_radix(len_str, 16).context("pkt hex length")?;
        if len < 4 {
            anyhow::bail!("invalid pkt length {len}");
        }
        let end = i + len;
        if end > body.len() {
            anyhow::bail!("truncated pkt-line at offset {i}");
        }
        let payload = &body[i + 4..end];
        let text = std::str::from_utf8(payload)
            .context("pkt payload utf8")?
            .trim_end_matches('\n')
            .to_owned();
        lines.push(text);
        i = end;
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::repo::Repository;
    use tempfile::TempDir;

    #[test]
    fn serve_clone_request_ends_with_done_and_flush() {
        let dir = TempDir::new().expect("tempdir");
        let status = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");
        let repo = Repository::discover(Some(dir.path())).expect("discover");
        let mut body = Vec::new();
        write_v0_serve_clone_request(&repo, &mut body).expect("write request");
        let lines = parse_v0_request_lines(&body).expect("parse");
        assert!(lines.iter().any(|l| l.starts_with("want ")));
        assert!(lines.iter().any(|l| l == "done"));
        let flush_count = lines.iter().filter(|l| l.is_empty()).count();
        assert_eq!(flush_count, 2, "expected two flush packets");
        let first_want = lines.iter().find(|l| l.starts_with("want ")).unwrap();
        assert!(first_want.contains("side-band-64k"));
        assert!(first_want.contains("thin-pack"));
        assert!(first_want.contains("ofs-delta"));
    }
}
