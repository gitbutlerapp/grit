//! History rewrites preserve change-id headers and store derived ids when missing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::change_id::{set_change_id, ChangeId, ChangeIdError, ChangeIdGenerator};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::hunk_select::{unstaged_file_diffs, HunkSelection};
use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    absorb, apply_absorb_rewrite, apply_fixup_hunk_selection, apply_rewrite, drop as history_drop,
    fixup, move_commit, rebase_onto, reword, split, squash, AbsorbOptions, ApplyOptions,
    EmptyPolicy, FixupOptions, Placement, RebaseOntoOptions, RefScope, RewriteOptions,
    SplitOptions, SquashMessage, SquashOptions,
};

use support::{commit_file, git, git_fsck_strict, git_rev_parse, HistoryRepo};

struct FixedGenerator([u8; 16]);

impl ChangeIdGenerator for FixedGenerator {
    fn random_bytes(&self) -> std::result::Result<[u8; 16], ChangeIdError> {
        Ok(self.0)
    }
}

const AUTHOR: &str = "Author <a@grit.test> 1700000000 +0000";

fn cid(hex: &str) -> ChangeId {
    ChangeId::parse_reverse_hex(hex).expect("reverse-hex id")
}

fn open_grit(wt: &Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
}

fn open_grit_with_policy(
    wt: &Path,
    change_ids_enabled: bool,
    generator: Option<Arc<dyn ChangeIdGenerator>>,
) -> Repository {
    let mut env = Environment::empty();
    if !change_ids_enabled {
        env.git_config_pairs
            .push(("grit.changeIds".into(), "false".into()));
    }
    let mut opts = RepositoryOptions::with_environment(env).with_work_tree(wt);
    if let Some(gen) = generator {
        opts = opts.with_change_id_generator(gen);
    }
    Repository::discover(Some(wt), &opts).expect("open")
}

fn git_cat_file_commit(wt: &Path, oid: &ObjectId) -> String {
    git(wt, &["cat-file", "-p", &oid.to_hex()])
}

fn assert_header_line(raw: &str, prefix: &str, want_value: &str) {
    let line = raw
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("missing {prefix} in commit:\n{raw}"));
    let value = line.strip_prefix(prefix).expect("prefix");
    assert_eq!(
        value, want_value,
        "unexpected {prefix} value in commit:\n{raw}"
    );
}

fn assert_change_id_on_oid(wt: &Path, oid: &ObjectId, want: &str) {
    let raw = git_cat_file_commit(wt, oid);
    assert_header_line(&raw, "change-id ", want);
    git_fsck_strict(wt);
}

fn write_tree_blob(repo: &Repository, path: &str, content: &[u8]) -> ObjectId {
    let blob = repo.odb().write(ObjectKind::Blob, content).expect("blob");
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: content.len() as u32,
        oid: blob,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    });
    write_tree_from_index(repo.odb(), &index, "").expect("tree")
}

fn grit_commit_with_id(
    repo: &Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    message: &str,
    change_id_hex: Option<&str>,
    extra_headers: &[u8],
) -> ObjectId {
    let mut data = CommitData {
        tree,
        parents: parents.to_vec(),
        author: AUTHOR.into(),
        committer: AUTHOR.into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: message.into(),
        raw_message: None,
        extra_headers: extra_headers.to_vec(),
    };
    if let Some(id) = change_id_hex {
        set_change_id(&mut data, &cid(id));
    }
    repo.odb()
        .write(ObjectKind::Commit, &serialize_commit(&data))
        .expect("write commit")
}

fn grit_linear(wt: &Path, grit: &Repository, messages: &[&str], ids: &[&str]) -> Vec<ObjectId> {
    assert_eq!(messages.len(), ids.len());
    let empty = grit.odb().write(ObjectKind::Tree, &[]).expect("empty");
    let mut parents: Vec<ObjectId> = Vec::new();
    let mut oids = Vec::new();
    for (msg, id) in messages.iter().zip(ids.iter()) {
        let oid = grit_commit_with_id(grit, empty, &parents, msg, Some(id), &[]);
        parents = vec![oid];
        oids.push(oid);
    }
    let tip = *oids.last().expect("tip");
    write_ref(grit.git_dir(), "refs/heads/main", &tip).expect("ref");
    git(wt, &["reset", "--hard", &tip.to_hex()]);
    oids
}

fn rewrite_opts(seq: i64) -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: format!("Rewriter <r@grit.test> {seq} +0000")
            .parse()
            .expect("signature"),
        preserve_committer: false,
        sign: true,
    }
}

fn apply_plan(grit: &Repository, plan: &grit_lib::revwalk::history::RewritePlan) {
    apply_rewrite(
        grit,
        plan,
        &ApplyOptions {
            identity: support::reflog_signature("Rewriter <r@grit.test>"),
            reflog_message: "history change-id test".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("apply");
}

fn replacement_id(plan: &grit_lib::revwalk::history::RewritePlan, old: ObjectId) -> ObjectId {
    plan.commits
        .iter()
        .find(|r| r.old == old)
        .and_then(|r| r.new)
        .unwrap_or(old)
}

#[test]
fn history_change_ids_survive_every_rewrite() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let empty = grit.odb().write(ObjectKind::Tree, &[]).expect("empty");

    // reword
    let chain = grit_linear(
        wt,
        &grit,
        &["a\n", "b\n", "c\n"],
        &[
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzy",
        ],
    );
    let b = chain[1];
    let plan = reword(&grit, b, "b reworded\n".into(), &rewrite_opts(1)).expect("reword");
    let new_b = replacement_id(&plan, b);
    apply_plan(&grit, &plan);
    assert_change_id_on_oid(wt, &new_b, "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy");

    // fixup (target keeps id)
    let tree = write_tree_blob(&grit, "fix.txt", b"base\n");
    let f1 = grit_commit_with_id(
        &grit,
        tree,
        &[],
        "f1\n",
        Some("wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww"),
        &[],
    );
    let f2 = grit_commit_with_id(
        &grit,
        tree,
        &[f1],
        "f2\n",
        Some("vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv"),
        &[],
    );
    write_ref(grit.git_dir(), "refs/heads/main", &f2).expect("ref");
    git(wt, &["reset", "--hard", &f2.to_hex()]);
    std::fs::write(wt.join("fix.txt"), b"base\nextra\n").expect("write");
    git(wt, &["add", "fix.txt"]);
    let plan = fixup(
        &grit,
        f1,
        &FixupOptions {
            message: None,
            empty: EmptyPolicy::Drop,
            rewrite: rewrite_opts(2),
            selection: None,
            pathspecs: Vec::new(),
        },
    )
    .expect("fixup");
    let new_f1 = replacement_id(&plan, f1);
    grit_lib::revwalk::history::apply_fixup_rewrite(
        &grit,
        &plan,
        &ApplyOptions {
            identity: support::reflog_signature("Rewriter <r@grit.test>"),
            reflog_message: "fixup".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &grit_lib::revwalk::history::staged_fixup_paths(&grit).expect("staged"),
    )
    .expect("apply fixup");
    assert_change_id_on_oid(wt, &new_f1, "wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww");

    // absorb
    let base_tree = write_tree_blob(&grit, "absorb.txt", b"one\n");
    let base = grit_commit_with_id(
        &grit,
        base_tree,
        &[],
        "base\n",
        Some("uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu"),
        &[],
    );
    let tip_tree = write_tree_blob(&grit, "absorb.txt", b"one\ntwo\n");
    let tip = grit_commit_with_id(
        &grit,
        tip_tree,
        &[base],
        "tip\n",
        Some("tttttttttttttttttttttttttttttttt"),
        &[],
    );
    write_ref(grit.git_dir(), "refs/heads/main", &tip).expect("ref");
    git(wt, &["reset", "--hard", &tip.to_hex()]);
    std::fs::write(wt.join("absorb.txt"), b"one\ntwo\nthree\n").expect("write");
    git(wt, &["add", "absorb.txt"]);
    let plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(base),
            rewrite: rewrite_opts(3),
        },
    )
    .expect("absorb");
    apply_absorb_rewrite(
        &grit,
        &plan,
        &ApplyOptions {
            identity: support::reflog_signature("Rewriter <r@grit.test>"),
            reflog_message: "absorb".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("apply absorb");
    let main = resolve_ref(grit.git_dir(), "refs/heads/main").expect("main");
    let parent = parse_commit(&grit.odb().read(&main).expect("read").data)
        .expect("parse")
        .parents[0];
    assert_change_id_on_oid(wt, &parent, "uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu");

    // move
    let moved = grit_linear(
        wt,
        &grit,
        &["m1\n", "m2\n", "m3\n", "m4\n"],
        &[
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzx",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzw",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzv",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzu",
        ],
    );
    let m2 = moved[1];
    let m3 = moved[2];
    let plan = move_commit(&grit, m3, Placement::Before(m2), &rewrite_opts(4)).expect("move");
    let new_m3 = replacement_id(&plan, m3);
    apply_plan(&grit, &plan);
    assert_change_id_on_oid(wt, &new_m3, "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzv");

    // drop (descendant replay keeps surviving ids)
    let dropped = grit_linear(
        wt,
        &grit,
        &["d1\n", "d2\n", "d3\n"],
        &[
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzt",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzs",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzr",
        ],
    );
    let d2 = dropped[1];
    let plan = history_drop(&grit, d2, &rewrite_opts(5)).expect("drop");
    apply_plan(&grit, &plan);
    let tip = resolve_ref(grit.git_dir(), "refs/heads/main").expect("tip");
    let replayed = parse_commit(&grit.odb().read(&tip).expect("read").data).expect("parse");
    assert_change_id_on_oid(wt, &replayed.parents[0], "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzt");

    // squash (oldest keeps id)
    let squashed = grit_linear(
        wt,
        &grit,
        &["s1\n", "s2\n", "s3\n"],
        &[
            "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk",
            "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkl",
            "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkm",
        ],
    );
    let s1 = squashed[0];
    let s3 = squashed[2];
    let plan = squash(
        &grit,
        &[&format!("{}..{}", s1.to_hex(), s3.to_hex())],
        &SquashOptions {
            message: SquashMessage::Oldest,
            rewrite: rewrite_opts(6),
        },
    )
    .expect("squash");
    let new_s1 = replacement_id(&plan, s1);
    apply_plan(&grit, &plan);
    assert_change_id_on_oid(wt, &new_s1, "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk");

    // split (original keeps, insert gets new id)
    std::fs::write(wt.join("split-a"), b"a").expect("write");
    std::fs::write(wt.join("split-b"), b"b").expect("write");
    git(wt, &["add", "split-a", "split-b"]);
    git(wt, &["commit", "-m", "split me"]);
    let head_git = git_rev_parse(wt, "HEAD").expect("head");
    let head_parsed = parse_commit(&grit.odb().read(&head_git).expect("read").data).expect("parse");
    let mut tagged = head_parsed;
    set_change_id(&mut tagged, &cid("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzq"));
    let head = grit
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&tagged))
        .expect("tag head");
    write_ref(grit.git_dir(), "refs/heads/main", &head).expect("ref");
    git(wt, &["reset", "--hard", &head.to_hex()]);
    let grit_split = open_grit_with_policy(wt, true, Some(Arc::new(FixedGenerator([0xab; 16]))));
    let plan = split(
        &grit_split,
        head,
        &["split-a".to_string()],
        "inserted\n".into(),
        &SplitOptions {
            rest_message: Some("rest\n".into()),
        },
        &rewrite_opts(7),
    )
    .expect("split");
    let new_head = replacement_id(&plan, head);
    apply_plan(&grit_split, &plan);
    assert_change_id_on_oid(wt, &new_head, "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzq");
    let inserted = parse_commit(&grit_split.odb().read(&new_head).expect("read").data)
        .expect("parse")
        .parents[0];
    assert_change_id_on_oid(wt, &inserted, ChangeId::from_bytes(&[0xab; 16]).as_str());

    // rebase_onto
    let r1 = grit_commit_with_id(
        &grit,
        empty,
        &[],
        "r1\n",
        Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzp"),
        &[],
    );
    let r2 = grit_commit_with_id(
        &grit,
        empty,
        &[r1],
        "r2\n",
        Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzo"),
        &[],
    );
    let r3 = grit_commit_with_id(
        &grit,
        empty,
        &[r2],
        "r3\n",
        Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzn"),
        &[],
    );
    write_ref(grit.git_dir(), "refs/heads/main", &r3).expect("ref");
    git(wt, &["reset", "--hard", &r3.to_hex()]);
    let onto = grit_commit_with_id(
        &grit,
        empty,
        &[r1],
        "onto\n",
        Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzm"),
        &[],
    );
    write_ref(grit.git_dir(), "refs/heads/onto", &onto).expect("onto ref");
    let plan = rebase_onto(
        &grit,
        r1,
        onto,
        &[r3],
        &RebaseOntoOptions {
            rewrite: rewrite_opts(8),
            skip_applied: true,
            flatten_leaf_merges: false,
            favor: grit_lib::diff::merge_file::MergeFavor::default(),
        },
    )
    .expect("rebase");
    let new_r3 = replacement_id(&plan, r3);
    apply_plan(&grit, &plan);
    assert_change_id_on_oid(wt, &new_r3, "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzn");
}

#[test]
fn rewrite_stores_derived_id_for_headerless_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    commit_file(wt, "f.txt", "x\n", "only");
    let old = git_rev_parse(wt, "HEAD").expect("head");
    let plan = reword(&grit, old, "reworded\n".into(), &rewrite_opts(1)).expect("plan");
    let new_oid = replacement_id(&plan, old);
    apply_plan(&grit, &plan);
    let want = ChangeId::derived_from_commit_id(&old).as_str().to_owned();
    assert_change_id_on_oid(wt, &new_oid, &want);
}

#[test]
fn rewrite_keeps_legacy_and_unknown_headers() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let tree = write_tree_blob(&grit, "f.txt", b"x\n");
    let extra = b"change-id jj-style-id\n\
gitbutler-change-id legacy-should-stay\n\
x-custom custom-value\n";
    let old = grit_commit_with_id(&grit, tree, &[], "hdr\n", None, extra);
    write_ref(grit.git_dir(), "refs/heads/main", &old).expect("ref");
    git(wt, &["reset", "--hard", &old.to_hex()]);
    let plan = reword(&grit, old, "reworded hdr\n".into(), &rewrite_opts(1)).expect("plan");
    let new_oid = replacement_id(&plan, old);
    apply_plan(&grit, &plan);
    let raw = git_cat_file_commit(wt, &new_oid);
    assert_header_line(&raw, "change-id ", "jj-style-id");
    assert_header_line(&raw, "gitbutler-change-id ", "legacy-should-stay");
    assert_header_line(&raw, "x-custom ", "custom-value");
    git_fsck_strict(wt);
}

#[test]
fn rewrite_strips_signature_but_keeps_change_id() {
    if !Command::new("ssh-keygen").arg("-V").output().is_ok() {
        eprintln!("SKIP: ssh-keygen unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path();
    git(wt, &["init", "-q", "-b", "main"]);
    let grit = open_grit(wt);
    let tree = write_tree_blob(&grit, "f.txt", b"x\n");
    let mut extra = b"change-id signed-change\n".to_vec();
    extra.extend_from_slice(
        b"gpgsig -----BEGIN SSH SIGNATURE-----\n sig\n -----END SSH SIGNATURE-----\n",
    );
    let old = grit_commit_with_id(&grit, tree, &[], "signed\n", None, &extra);
    write_ref(grit.git_dir(), "refs/heads/main", &old).expect("ref");
    git(wt, &["reset", "--hard", &old.to_hex()]);
    let plan = reword(&grit, old, "still signed id\n".into(), &rewrite_opts(1)).expect("plan");
    let new_oid = replacement_id(&plan, old);
    apply_plan(&grit, &plan);
    let raw = git_cat_file_commit(wt, &new_oid);
    assert_header_line(&raw, "change-id ", "signed-change");
    assert!(
        !raw.contains("gpgsig"),
        "signature should be stripped: {raw}"
    );
    git_fsck_strict(wt);
}

#[test]
fn change_ids_disabled_adds_nothing() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt);
    commit_file(wt, "f.txt", "x\n", "plain");
    let old = git_rev_parse(wt, "HEAD").expect("head");
    let plan = reword(&grit, old, "no id\n".into(), &rewrite_opts(1)).expect("plan");
    let new_oid = replacement_id(&plan, old);
    apply_plan(&grit, &plan);
    let raw = git_cat_file_commit(wt, &new_oid);
    assert!(
        !raw.lines()
            .any(|l| l.starts_with("change-id ") || l.starts_with("gitbutler-change-id ")),
        "unexpected change id header: {raw}"
    );
    git_fsck_strict(wt);
}

#[test]
fn fixup_hunks_preserves_change_id() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let tree = write_tree_blob(&grit, "pick.txt", b"base\n");
    let pick1 = grit_commit_with_id(
        &grit,
        tree,
        &[],
        "pick1\n",
        Some("pppppppppppppppppppppppppppppppp"),
        &[],
    );
    let pick2 = grit_commit_with_id(
        &grit,
        tree,
        &[pick1],
        "pick2\n",
        Some("qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"),
        &[],
    );
    write_ref(grit.git_dir(), "refs/heads/main", &pick2).expect("ref");
    git(wt, &["reset", "--hard", &pick2.to_hex()]);
    std::fs::write(wt.join("pick.txt"), b"base\nextra\n").expect("write");
    let files = unstaged_file_diffs(&grit, &[]).expect("diff");
    let (file, peer) = &files[0];
    let ids = hunk_ids_with_duplicate_peers(DiffSide::Unstaged, &file.path, &file.hunks, peer);
    let selection = HunkSelection {
        ids: vec![ids[0].clone()],
        ranges: Vec::new(),
    };
    let plan = fixup(
        &grit,
        pick1,
        &FixupOptions {
            message: None,
            empty: EmptyPolicy::Drop,
            rewrite: rewrite_opts(1),
            selection: Some(selection.clone()),
            pathspecs: Vec::new(),
        },
    )
    .expect("fixup hunks");
    let new_pick1 = replacement_id(&plan, pick1);
    apply_fixup_hunk_selection(
        &grit,
        &plan,
        &ApplyOptions {
            identity: support::reflog_signature("Rewriter <r@grit.test>"),
            reflog_message: "fixup hunks".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &selection,
        &[],
    )
    .expect("apply fixup hunks");
    assert_change_id_on_oid(wt, &new_pick1, "pppppppppppppppppppppppppppppppp");
    git_fsck_strict(wt);
}
