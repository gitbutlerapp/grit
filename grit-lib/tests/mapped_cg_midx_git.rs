//! Git-written commit-graph and MIDX: mapped file backing matches owned-byte parsing.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::commit_graph::{
    commit_graph_chain_from_layers_for_test, CommitGraphChain, CommitGraphLayer,
};
use grit_lib::objects::midx::{midx_lookup_pack_and_offset_opt, try_read_object_via_midx};
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::ObjectId;

fn git_ok() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).expect("utf8")
}

fn git(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

#[test]
fn git_commit_graph_chain_mapped_matches_owned_layers() {
    if !git_ok() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-q", "-m", "c1"]);
    git(dir.path(), &["commit", "--allow-empty", "-q", "-m", "c2"]);
    git(dir.path(), &["commit-graph", "write", "--reachable"]);

    let objects = dir.path().join(".git/objects");
    let mapped = CommitGraphChain::try_load(&objects)
        .expect("load")
        .expect("chain");
    let mut owned_layers = Vec::new();
    for path in mapped.layer_paths_oldest_first() {
        let raw = std::fs::read(&path).expect("read layer");
        owned_layers.push(CommitGraphLayer::try_parse(path, raw).expect("owned layer"));
    }
    owned_layers.reverse();
    let owned = commit_graph_chain_from_layers_for_test(owned_layers);
    assert_eq!(mapped.num_layers(), owned.num_layers());
    assert_eq!(mapped.total_commits(), owned.total_commits());
    for oid in mapped.all_oids_in_order() {
        assert_eq!(mapped.graph_commit(&oid), owned.graph_commit(&oid));
        assert_eq!(
            mapped.commit_generation_for_walk(&oid),
            owned.commit_generation_for_walk(&oid)
        );
    }
}

#[test]
fn git_midx_mapped_read_matches_owned_byte_lookup() {
    if !git_ok() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f.txt"), b"x\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c1"]);
    git(dir.path(), &["repack", "-adf"]);
    git(dir.path(), &["multi-pack-index", "write"]);

    let objects = dir.path().join(".git/objects");
    let head_hex = git_out(dir.path(), &["rev-parse", "HEAD"]);
    let oid = ObjectId::from_hex(head_hex.trim()).expect("head oid");

    let via_midx_mapped = try_read_object_via_midx(&objects, &oid)
        .expect("read")
        .expect("object");
    grit_lib::objects::midx::evict_midx_read_cache_for_pack_dir(&objects.join("pack"));
    clear_pack_cache();

    let pack_dir = objects.join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    std::fs::write(&midx_path, std::fs::read(&midx_path).expect("copy midx")).expect("rewrite");
    let via_midx_reload = try_read_object_via_midx(&objects, &oid)
        .expect("read after rewrite")
        .expect("object after rewrite");
    assert_eq!(via_midx_mapped.data, via_midx_reload.data);
    assert_eq!(
        midx_lookup_pack_and_offset_opt(&objects, &oid).expect("lookup"),
        midx_lookup_pack_and_offset_opt(&objects, &oid).expect("lookup again")
    );
}
