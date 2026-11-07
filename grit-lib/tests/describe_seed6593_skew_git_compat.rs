//! Skewed merge DAG without commit-graph: `RevWalk::describe` matches system
//! `git rev-list --count` for nearest-tag distance (seed-6593 regression).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::{serialize_tag, ObjectId, ObjectKind, TagData};
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::DescribeOptions;

mod support;
use support::{git, git_interop_available, Backend};

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

fn write_commit_at(
    odb: &grit_lib::objects::odb::Odb,
    parents: &[ObjectId],
    msg: &str,
    time: i64,
) -> grit_lib::error::Result<ObjectId> {
    let tree = odb.write_loose_materialize(ObjectKind::Tree, b"")?;
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str(&format!(
        "author T <t@e.com> {time} +0000\ncommitter T <t@e.com> {time} +0000\n\n{msg}\n"
    ));
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn tag_annotated(repo: &Repository, name: &str, target: ObjectId, msg: &str) {
    let tagger = grit_lib::ident::Signature::new("T", "t@e.com", repo.identity_now()).expect("sig");
    let data = TagData {
        object: target,
        object_type: "commit".into(),
        tag: name.to_owned(),
        tagger: Some(tagger.header_line()),
        extra_headers: Vec::new(),
        message: format!("{msg}\n"),
        raw_message: None,
    };
    let bytes = serialize_tag(&data);
    let oid = repo
        .odb()
        .write(ObjectKind::Tag, &bytes)
        .expect("tag write");
    let full = format!("refs/tags/{name}");
    let name = RefName::try_from(full.as_str()).expect("ref");
    repo.references().update(&name, &oid).expect("update");
}

/// Deterministic merge-heavy DAG: 150 commits, 15 annotated tags, skewed committer dates.
fn build_seed6593_fixture() -> (tempfile::TempDir, Repository, Vec<ObjectId>, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let odb = repo.odb();
    let mut seed = 6593u64;
    let root = write_commit_at(odb, &[], "root", 5000).expect("root");
    let mut commits = vec![root];
    while commits.len() < 150 {
        let i = (lcg(&mut seed) as usize) % commits.len();
        let j = (lcg(&mut seed) as usize) % commits.len();
        let time = 1000 + (lcg(&mut seed) % 50_000) as i64;
        let parents = if i == j {
            vec![commits[i]]
        } else {
            vec![commits[i], commits[j]]
        };
        let c =
            write_commit_at(odb, &parents, &format!("c{}", commits.len()), time).expect("commit");
        commits.push(c);
    }
    let mut tag_names = Vec::new();
    for t in 0..15 {
        let idx = (lcg(&mut seed) as usize) % commits.len();
        let name = format!("t{t:02}");
        tag_annotated(&repo, &name, commits[idx], "m");
        tag_names.push(name);
    }
    (dir, repo, commits, tag_names)
}

fn git_rev_list_count(wt: &std::path::Path, head: &ObjectId, tag: &str) -> usize {
    let head_hex = head.to_hex().to_string();
    git(
        wt,
        &[
            "rev-list",
            "--count",
            &format!("refs/tags/{tag}..{head_hex}"),
        ],
    )
    .trim()
    .parse::<usize>()
    .expect("git count")
}

#[test]
fn describe_seed6593_skewed_merge_matches_git_rev_list_without_commit_graph() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let (dir, repo, commits, tag_names) = build_seed6593_fixture();
    let wt = dir.path();
    let opts = DescribeOptions::default();
    let mut sample_seed = 6593u64 ^ 0xDEAD_BEEF;
    for sample in 0..30 {
        let idx = (lcg(&mut sample_seed) as usize) % commits.len();
        let head = commits[idx];
        let head_hex = head.to_hex().to_string();
        grit_lib::refs::write_ref(repo.git_dir(), "refs/heads/main", &head).expect("ref");
        let mut min_dist = usize::MAX;
        let mut has_ancestor_tag = false;
        for name in &tag_names {
            let peel = repo
                .references()
                .resolve(&RefName::try_from(format!("refs/tags/{name}").as_str()).expect("ref"))
                .expect("resolve");
            let peel_commit = repo
                .odb()
                .peel_to_commit(peel)
                .expect("peel")
                .expect("commit");
            if !repo
                .revwalk()
                .is_ancestor(peel_commit, head)
                .expect("ancestor")
            {
                continue;
            }
            has_ancestor_tag = true;
            min_dist = min_dist.min(git_rev_list_count(wt, &head, name));
        }
        if !has_ancestor_tag {
            assert!(
                repo.revwalk()
                    .describe(&head_hex, &opts)
                    .expect("describe")
                    .is_none(),
                "sample {sample} head {head_hex}: expected no ancestor tag"
            );
            continue;
        }
        let described = repo
            .revwalk()
            .describe(&head_hex, &opts)
            .expect("describe")
            .unwrap_or_else(|| panic!("describe None for sample {sample} head {head_hex}"));
        let grit_tag = described
            .tag
            .as_str()
            .strip_prefix("refs/tags/")
            .unwrap_or(described.tag.as_str());
        let git_dist_for_grit = git_rev_list_count(wt, &head, grit_tag);
        assert_eq!(
            described.distance, git_dist_for_grit,
            "sample {sample} head {head_hex}: distance mismatch for tag {grit_tag}"
        );
        assert_eq!(
            described.distance, min_dist,
            "sample {sample} head {head_hex}: grit did not pick minimum git rev-list distance among ancestor tags"
        );
    }
}
