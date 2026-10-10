//! Stateless v0 shallow upload-pack round-trip (two POST bodies).

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::pkt_line;
use grit_lib::repo::Repository;
use grit_protocol::upload_pack::stateless_rpc;
use grit_protocol::RepositoryOptions;

fn init_linear_bare(dir: &std::path::Path, commits: usize) -> ObjectId {
    grit_lib::repo::init_bare_clone_minimal(
        dir,
        "main",
        grit_lib::ref_storage::RefStorageFormat::Files,
    )
    .expect("init bare");
    let repo = Repository::open_for_serving(dir, &RepositoryOptions::default()).expect("open");
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
            .odb
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
    grit_lib::repo::init_bare_clone_minimal(
        &dir,
        "main",
        grit_lib::ref_storage::RefStorageFormat::Files,
    )
    .expect("init bare");
    let repo = Repository::open_for_serving(&dir, &RepositoryOptions::default()).expect("open");
    let tree = grit_lib::objects::ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
        .expect("empty tree");
    let commit_body = format!(
        "tree {}\nauthor T <t@x> 1 +0000\ncommitter T <t@x> 1 +0000\n\nm\n",
        tree.to_hex()
    );
    let main = repo
        .odb
        .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    grit_lib::refs::write_ref(&dir, "refs/heads/main", &main).expect("ref");

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
