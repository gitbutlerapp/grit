//! Stateless v0 shallow upload-pack round-trip (two POST bodies).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::remote::pkt_line;
use grit_lib::repo::{InitOptions, Repository};
use grit_protocol::upload_pack::stateless_rpc;
use std::process::Command;

fn init_linear_bare(dir: &std::path::Path, commits: usize) -> ObjectId {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init bare");
    let repo = Repository::open(dir, &RepositoryOptions::default().for_serving()).expect("open");
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
    let mut parent: Option<ObjectId> = None;
    let mut tip = tree;
    for i in 0..commits {
        let parents = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
        let commit_body = format!(
            "tree {tree}\n{parents}author T <t@x> 1 +0000\ncommitter T <t@x> 1 +0000\n\nc{i}\n",
            tree = tree.to_hex(),
        );
        tip = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())
            .expect("commit");
        parent = Some(tip);
    }
    grit_lib::refs::write_ref(dir, "refs/heads/main", &tip).expect("ref");
    tip
}

#[test]
fn two_post_shallow_depth_one_returns_pack_on_done() {
    let dir = std::env::temp_dir().join(format!("grit-shallow-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let main = init_linear_bare(&dir, 1);

    let caps = " multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta";
    let mut round1 = Vec::new();
    pkt_line::write_line_to_vec(&mut round1, &format!("want {}{}", main.to_hex(), caps)).unwrap();
    pkt_line::write_line_to_vec(&mut round1, "deepen 1").unwrap();
    round1.extend_from_slice(b"0000");

    let out1 = stateless_rpc(&dir, &round1, None, &RepositoryOptions::default()).expect("round1");
    let out1_text = String::from_utf8_lossy(&out1);
    assert!(
        out1_text.contains("shallow "),
        "round1 shallow-info: {out1_text}"
    );

    let mut round2 = round1.clone();
    pkt_line::write_line_to_vec(&mut round2, "done").unwrap();
    let out2 = stateless_rpc(&dir, &round2, None, &RepositoryOptions::default()).expect("round2");
    let out2_text = String::from_utf8_lossy(&out2);
    assert!(
        out2_text.contains("shallow "),
        "round2 must repeat shallow lines before pack: {out2_text}"
    );
    assert!(
        out2.windows(4).any(|w| w == b"PACK"),
        "round2 must contain PACK (sideband-wrapped ok): len={} bytes",
        out2.len()
    );
}

#[test]
fn single_post_shallow_depth_one_double_flush_before_done() {
    let dir = std::env::temp_dir().join(format!("grit-shallow-df-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let main = init_linear_bare(&dir, 5);

    let caps = " multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta";
    let mut body = Vec::new();
    pkt_line::write_line_to_vec(&mut body, &format!("want {}{}", main.to_hex(), caps)).unwrap();
    pkt_line::write_line_to_vec(&mut body, "deepen 1").unwrap();
    body.extend_from_slice(b"0000");
    body.extend_from_slice(b"0000");
    pkt_line::write_line_to_vec(&mut body, "done").unwrap();
    body.extend_from_slice(b"0000");

    let out = stateless_rpc(&dir, &body, None, &RepositoryOptions::default()).expect("rpc");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("shallow "),
        "response must include shallow lines before pack: {text}"
    );
    assert!(
        out.windows(4).any(|w| w == b"PACK"),
        "response must contain PACK: {text}"
    );
}

fn v0_data_lines(out: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pos = 0;
    while pos + 4 <= out.len() {
        let len = u16::from_str_radix(
            std::str::from_utf8(&out[pos..pos + 4]).unwrap_or("0000"),
            16,
        )
        .unwrap_or(0) as usize;
        if len == 0 {
            pos += 4;
            continue;
        }
        if len < 4 || pos + len > out.len() {
            break;
        }
        let payload = &out[pos + 4..pos + len];
        if let Ok(text) = std::str::from_utf8(payload) {
            lines.push(text.trim_end_matches('\n').to_owned());
        }
        pos += len;
    }
    lines
}

fn first_line_prefix(lines: &[String], prefix: &str) -> Option<usize> {
    lines.iter().position(|l| l.starts_with(prefix))
}

#[test]
fn deepen_with_haves_puts_shallow_section_before_acks() {
    let dir = std::env::temp_dir().join(format!("grit-shallow-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let main = init_linear_bare(&dir, 8);

    let caps = " multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta";
    let mut body = Vec::new();
    pkt_line::write_line_to_vec(&mut body, &format!("want {}{}", main.to_hex(), caps)).unwrap();
    let repo = Repository::open(&dir, &RepositoryOptions::default().for_serving()).unwrap();
    let shallow_boundary = repo.odb().read_commit(&main).unwrap().parents[0];
    pkt_line::write_line_to_vec(&mut body, &format!("shallow {}", shallow_boundary.to_hex()))
        .unwrap();
    pkt_line::write_line_to_vec(&mut body, "deepen 5").unwrap();
    body.extend_from_slice(b"0000");
    let mut have = shallow_boundary;
    for _ in 0..3 {
        pkt_line::write_line_to_vec(&mut body, &format!("have {}", have.to_hex())).unwrap();
        have = repo.odb().read_commit(&have).unwrap().parents[0];
    }
    pkt_line::write_line_to_vec(&mut body, "done").unwrap();
    body.extend_from_slice(b"0000");

    let out = stateless_rpc(&dir, &body, None, &RepositoryOptions::default()).expect("rpc");
    let lines = v0_data_lines(&out);
    let shallow_at = first_line_prefix(&lines, "shallow ")
        .or_else(|| first_line_prefix(&lines, "unshallow "))
        .expect("shallow section");
    let ack_at = first_line_prefix(&lines, "ACK ").expect("ACK lines");
    assert!(
        shallow_at < ack_at,
        "shallow section must precede ACKs: lines={lines:?}"
    );
    let shallow_before_ack = lines
        .iter()
        .take(ack_at)
        .filter(|l| l.starts_with("shallow ") || l.starts_with("unshallow "))
        .count();
    assert!(
        shallow_before_ack >= 1,
        "expected shallow/unshallow before ACKs: {lines:?}"
    );
    let shallow_after_ack = lines
        .iter()
        .skip(ack_at)
        .filter(|l| l.starts_with("shallow ") || l.starts_with("unshallow "))
        .count();
    assert_eq!(
        shallow_after_ack, 0,
        "must not repeat shallow section after ACKs: {lines:?}"
    );
}

#[test]
fn deepen_stateless_response_prefix_matches_git() {
    let dir = std::env::temp_dir().join(format!("grit-shallow-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let main = init_linear_bare(&dir, 6);

    let caps = " multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta";
    let mut body = Vec::new();
    pkt_line::write_line_to_vec(&mut body, &format!("want {}{}", main.to_hex(), caps)).unwrap();
    pkt_line::write_line_to_vec(&mut body, "deepen 3").unwrap();
    body.extend_from_slice(b"0000");
    pkt_line::write_line_to_vec(&mut body, "done").unwrap();
    body.extend_from_slice(b"0000");

    let grit_out = stateless_rpc(&dir, &body, None, &RepositoryOptions::default()).expect("grit");
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["upload-pack", "--stateless-rpc", dir.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&body).expect("write body");
    }
    let git_out = child.wait_with_output().expect("git output");
    assert!(
        git_out.status.success(),
        "git upload-pack failed: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );

    let grit_lines = v0_data_lines(&grit_out);
    let git_lines = v0_data_lines(&git_out.stdout);
    let grit_shallow: Vec<_> = grit_lines
        .iter()
        .take_while(|l| !l.starts_with("ACK ") && *l != "NAK")
        .filter(|l| l.starts_with("shallow ") || l.starts_with("unshallow "))
        .collect();
    let git_shallow: Vec<_> = git_lines
        .iter()
        .take_while(|l| !l.starts_with("ACK ") && *l != "NAK")
        .filter(|l| l.starts_with("shallow ") || l.starts_with("unshallow "))
        .collect();
    assert_eq!(
        grit_shallow, git_shallow,
        "shallow lines before ACK must match git"
    );
}
