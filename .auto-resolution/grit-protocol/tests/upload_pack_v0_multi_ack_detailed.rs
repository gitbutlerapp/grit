//! Protocol v0 `multi_ack_detailed`: terminal ACK after `done` must not use `ready`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::remote::pkt_line;
use grit_lib::repo::{InitOptions, Repository};
use grit_protocol::upload_pack::stateless_rpc;

fn write_commit(
    repo: &Repository,
    tree: ObjectId,
    parent: Option<ObjectId>,
    msg: &str,
) -> ObjectId {
    let parent_line = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
    let body = format!(
        "tree {tree}\n{parent_line}author T <t@x> 1 +0000\ncommitter T <t@x> 1 +0000\n\n{msg}\n",
    );
    repo.odb()
        .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}

#[test]
fn stateless_v0_done_emits_bare_ack_before_sideband_pack() {
    let dir =
        std::env::temp_dir().join(format!("grit-v0-multi-ack-detailed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Repository::init(
        &dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init bare");
    let repo = Repository::open(&dir, &RepositoryOptions::default().for_serving()).expect("open");
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    let parent = write_commit(&repo, tree, None, "root");
    let tip = write_commit(&repo, tree, Some(parent), "child");
    grit_lib::refs::write_ref(&dir, "refs/heads/main", &tip).expect("ref");

    let caps = " multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta";
    let mut body = Vec::new();
    pkt_line::write_line_to_vec(&mut body, &format!("want {}{}", tip.to_hex(), caps)).unwrap();
    body.extend_from_slice(b"0000");
    pkt_line::write_line_to_vec(&mut body, &format!("have {}", parent.to_hex())).unwrap();
    pkt_line::write_line_to_vec(&mut body, "done").unwrap();

    let out = stateless_rpc(&dir, &body, None, &RepositoryOptions::default()).expect("upload-pack");

    let text = String::from_utf8_lossy(&out);
    assert!(
        !text.contains(" ready\n") && !text.contains(" ready\r"),
        "terminal post-done ACK must not use ready: {text}"
    );
    let bare_ack = format!("ACK {}\n", parent.to_hex());
    assert!(
        text.contains(&bare_ack),
        "expected bare terminal ACK for common commit: {text}"
    );
    assert!(
        out.windows(4).any(|w| w == b"PACK"),
        "response must include a pack after negotiation"
    );
}
