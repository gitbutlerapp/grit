//! Stateless v0 shallow upload-pack round-trip (two POST bodies).

use grit_lib::objects::ObjectKind;
use grit_lib::pkt_line;
use grit_lib::repo::Repository;
use grit_protocol::upload_pack::stateless_rpc;
use grit_protocol::RepositoryOptions;

#[test]
fn two_post_shallow_depth_one_returns_pack_on_done() {
    let dir = std::env::temp_dir().join(format!("grit-shallow-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    grit_lib::repo::init_bare_clone_minimal(&dir, "main", "files").expect("init bare");
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
    assert!(
        out1.starts_with(b"0000") || out1.iter().copied().any(|b| b == b's'),
        "round1 shallow-info: {out1:?}"
    );

    let mut round2 = round1.clone();
    pkt_line::write_line_to_vec(&mut round2, "done").unwrap();
    let out2 = stateless_rpc(&dir, &round2, None, &RepositoryOptions::default()).expect("round2");
    assert!(
        out2.windows(4).any(|w| w == b"PACK"),
        "round2 must contain PACK (sideband-wrapped ok): len={} bytes",
        out2.len()
    );
}
