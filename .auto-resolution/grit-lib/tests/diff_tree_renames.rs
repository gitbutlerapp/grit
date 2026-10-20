//! Tree, index, and worktree diffs with rename/copy/break vs system `git` raw output.

mod support;

use std::fs;
use std::path::Path;

use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_trees, diff_trees_show_tree_entries, diff_trees_with_renames, DiffPostprocessOptions,
    GIT_DIFF_DEFAULT_BREAK_SCORE,
};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;

use support::{
    assert_raw_matches_git, commits_diff_raw, config_with_kv, git_quiet, git_raw_lines,
    grit_raw_lines_abbrev, head_tree_oid, index_to_tree_raw, index_to_worktree_raw, rev_parse_tree,
    tree_diff_raw, tree_to_worktree_raw_abbrev, write_bogus_tree, DiffRepo,
};

fn default_opts() -> DiffPostprocessOptions {
    DiffPostprocessOptions {
        rename_threshold: grit_lib::diff::RENAME_DETECTION_DISABLED,
        ..Default::default()
    }
}

fn rename_opts(threshold: u32) -> DiffPostprocessOptions {
    DiffPostprocessOptions {
        rename_threshold: threshold,
        ..Default::default()
    }
}

#[test]
fn t4002_tree_index_worktree_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("a.txt"), "alpha\n").unwrap();
    fs::write(wt.join("b.txt"), "beta\n").unwrap();
    git_quiet(wt, &["add", "."]);
    git_quiet(wt, &["commit", "-qm", "base"]);

    let tree_o = rev_parse_tree(wt, "HEAD^{tree}");
    fs::write(wt.join("a.txt"), "alpha2\n").unwrap();
    fs::write(wt.join("c.txt"), "new\n").unwrap();
    git_quiet(wt, &["add", "a.txt", "c.txt"]);
    let tree_a: ObjectId = git_quiet(wt, &["write-tree"]).trim().parse().unwrap();

    let config = ConfigSet::new();
    let opts = default_opts();

    let git_tree = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--raw",
            &tree_o.to_hex(),
            &tree_a.to_hex(),
        ],
    );
    let grit_tree = tree_diff_raw(&dr.repo, Some(&tree_o), Some(&tree_a), &config, &opts);
    assert_raw_matches_git(&grit_tree, &git_tree, "tree↔tree");

    let git_index = git_raw_lines(
        wt,
        &[
            "diff-index",
            "-r",
            "--cached",
            "--root",
            "--raw",
            &tree_o.to_hex(),
        ],
    );
    let index = dr.load_index();
    let grit_index = index_to_tree_raw(&dr.repo, &index, &tree_o, &config, &opts);
    assert_raw_matches_git(&grit_index, &git_index, "index↔tree (cached)");

    fs::write(wt.join("b.txt"), "beta-worktree\n").unwrap();
    let git_files = git_raw_lines(wt, &["diff-files", "-r", "--raw"]);
    let mut index = dr.load_index();
    let head_tree = head_tree_oid(&dr.repo);
    let grit_files =
        index_to_worktree_raw(&dr.repo, wt, &mut index, head_tree.as_ref(), &config, &opts);
    assert_raw_matches_git(&grit_files, &git_files, "index↔worktree");
}

#[test]
fn t4001_rename_detection_scores_match_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let path0: String = (1..=15)
        .map(|i| {
            if i == 11 {
                "line 11".to_string()
            } else {
                format!("Line {i}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(wt.join("path0"), format!("{path0}\n")).unwrap();
    git_quiet(wt, &["add", "path0"]);
    git_quiet(wt, &["commit", "-qm", "base"]);

    let mut edited = path0.replace("line 11", "Line 11");
    fs::remove_file(wt.join("path0")).unwrap();
    fs::write(wt.join("path1"), format!("{edited}\n")).unwrap();
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "rename"]);

    let parent = rev_parse_tree(wt, "HEAD~1^{tree}");
    let commit = rev_parse_tree(wt, "HEAD^{tree}");
    let config = ConfigSet::new();
    let opts = rename_opts(50);

    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "-M50",
            "--root",
            "--raw",
            &parent.to_hex(),
            &commit.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&parent), Some(&commit), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4001 rename scores");
}

#[test]
fn t4007_copy_detection_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::create_dir_all(wt.join("path0")).unwrap();
    fs::create_dir_all(wt.join("path1")).unwrap();
    let content = "copy me\nline two\n";
    fs::write(wt.join("path0/COPYING"), content).unwrap();
    git_quiet(wt, &["add", "path0/COPYING"]);
    let tree = git_quiet(wt, &["write-tree"])
        .trim()
        .parse::<ObjectId>()
        .unwrap();

    fs::write(wt.join("path1/COPYING"), content).unwrap();
    git_quiet(wt, &["add", "path1/COPYING"]);

    let config = ConfigSet::new();
    let opts = DiffPostprocessOptions {
        rename_threshold: 50,
        copy_threshold: Some(50),
        find_copies_harder: true,
        ..Default::default()
    };

    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-index",
            "-r",
            "-C50",
            "--find-copies-harder",
            "--root",
            "--raw",
            &tree.to_hex(),
        ],
    );
    let index = dr.load_index();
    let grit_lines = index_to_tree_raw(&dr.repo, &index, &tree, &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4007 copy harder");
}

fn large_dissimilar_payload(seed: &str) -> String {
    let mut s = String::with_capacity(820);
    s.push_str(seed);
    s.push('\n');
    while s.len() < 800 {
        s.push_str("padding line for break rewrite similarity test\n");
    }
    s
}

#[test]
fn t4008_break_rewrite_index_worktree_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let original = large_dissimilar_payload("index-worktree break seed");
    fs::write(wt.join("big.txt"), &original).unwrap();
    git_quiet(wt, &["add", "big.txt"]);
    git_quiet(wt, &["commit", "-qm", "base"]);

    let mut rewritten = original.clone();
    rewritten.replace_range(0..400, &"X".repeat(400));
    fs::write(wt.join("big.txt"), &rewritten).unwrap();

    let config = ConfigSet::new();
    let opts = DiffPostprocessOptions {
        break_score: Some(GIT_DIFF_DEFAULT_BREAK_SCORE),
        rename_threshold: grit_lib::diff::RENAME_DETECTION_DISABLED,
        ..Default::default()
    };

    let git_lines = git_raw_lines(wt, &["diff-files", "-r", "-B", "--raw"]);
    let mut index = dr.load_index();
    let head_tree = head_tree_oid(&dr.repo);
    let grit_lines =
        index_to_worktree_raw(&dr.repo, wt, &mut index, head_tree.as_ref(), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4008 break index↔worktree");
}

#[test]
fn t4008_break_rewrite_tree_worktree_rename_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let f_body = format!(
        "dissimilar contents in f\n{}",
        "f padding line\n".repeat(40)
    );
    let g_body = format!(
        "totally different body in g\n{}",
        "g padding line\n".repeat(40)
    );
    fs::write(wt.join("f"), &f_body).unwrap();
    fs::write(wt.join("g"), &g_body).unwrap();
    git_quiet(wt, &["add", "f", "g"]);
    git_quiet(wt, &["commit", "-qm", "two files"]);
    let tree = rev_parse_tree(wt, "HEAD^{tree}");

    let g_blob = fs::read_to_string(wt.join("g")).unwrap();
    fs::write(wt.join("f"), &g_blob).unwrap();
    fs::remove_file(wt.join("g")).unwrap();

    let config = ConfigSet::new();
    let opts = DiffPostprocessOptions {
        break_score: Some(GIT_DIFF_DEFAULT_BREAK_SCORE),
        rename_threshold: 50,
        materialize_break_worktree_new_oid: true,
        ..Default::default()
    };

    let git_lines = git_raw_lines(
        wt,
        &[
            "diff",
            "-r",
            "-B",
            "-M50",
            "--raw",
            &format!("{}:", tree.to_hex()),
        ],
    );
    let index = dr.load_index();
    let grit_lines =
        tree_to_worktree_raw_abbrev(&dr.repo, wt, &index, Some(&tree), &config, &opts, 7);
    assert_raw_matches_git(
        &grit_lines,
        &git_lines,
        "t4008 break+rename tree↔worktree (g→f)",
    );
    assert_eq!(
        grit_lines.len(),
        1,
        "git collapses break-split delete with rename into one R100 line"
    );
}

#[test]
fn t4008_break_rewrite_tree_worktree_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let original = large_dissimilar_payload("tree-worktree break seed");
    fs::write(wt.join("big.txt"), &original).unwrap();
    git_quiet(wt, &["add", "big.txt"]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    let tree = rev_parse_tree(wt, "HEAD^{tree}");

    let mut rewritten = original.clone();
    rewritten.replace_range(100..500, &"Y".repeat(400));
    fs::write(wt.join("big.txt"), &rewritten).unwrap();

    let config = ConfigSet::new();
    let opts = DiffPostprocessOptions {
        break_score: Some(GIT_DIFF_DEFAULT_BREAK_SCORE),
        rename_threshold: grit_lib::diff::RENAME_DETECTION_DISABLED,
        materialize_break_worktree_new_oid: true,
        ..Default::default()
    };

    let git_lines = git_raw_lines(
        wt,
        &["diff", "-r", "-B", "--raw", &format!("{}:", tree.to_hex())],
    );
    let index = dr.load_index();
    let grit_lines =
        tree_to_worktree_raw_abbrev(&dr.repo, wt, &index, Some(&tree), &config, &opts, 7);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4008 break tree↔worktree");
}

#[test]
fn t4008_break_rewrite_tree_worktree_eol_lf_crlf_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
    let original = large_dissimilar_payload("crlf tree-worktree break seed");
    fs::write(wt.join("f.txt"), &original).unwrap();
    git_quiet(wt, &["add", "."]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    let tree = rev_parse_tree(wt, "HEAD^{tree}");

    let mut rewritten = original.clone();
    rewritten.replace_range(100..500, &"Z".repeat(400));
    // Dissimilar rewrite with CRLF on disk; Git clean-smudges to LF for break scoring and OID.
    let mut crlf: Vec<u8> = rewritten.into_bytes();
    for b in crlf.iter_mut() {
        if *b == b'\n' {
            *b = b'\r';
        }
    }
    crlf.extend_from_slice(b"\r\n");
    fs::write(wt.join("f.txt"), &crlf).unwrap();

    let config = ConfigSet::new();
    let opts = DiffPostprocessOptions {
        break_score: Some(GIT_DIFF_DEFAULT_BREAK_SCORE),
        rename_threshold: grit_lib::diff::RENAME_DETECTION_DISABLED,
        materialize_break_worktree_new_oid: true,
        ..Default::default()
    };

    let git_lines = git_raw_lines(
        wt,
        &["diff", "-r", "-B", "--raw", &format!("{}:", tree.to_hex())],
    );
    let index = dr.load_index();
    let grit_lines =
        tree_to_worktree_raw_abbrev(&dr.repo, wt, &index, Some(&tree), &config, &opts, 7);
    assert_raw_matches_git(
        &grit_lines,
        &git_lines,
        "t4008 break tree↔worktree with eol=lf and CRLF worktree file",
    );
}

#[test]
fn t4008_break_rewrite_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("file0"), "some dissimilar content\n").unwrap();
    fs::write(
        wt.join("file1"),
        "totally different long body\nsecond line\n",
    )
    .unwrap();
    git_quiet(wt, &["add", "file0", "file1"]);
    let reference = git_quiet(wt, &["write-tree"])
        .trim()
        .parse::<ObjectId>()
        .unwrap();

    let body = fs::read_to_string(wt.join("file0"))
        .unwrap()
        .replace("some", "SOME");
    fs::write(wt.join("file1"), body).unwrap();
    fs::remove_file(wt.join("file0")).unwrap();
    git_quiet(wt, &["add", "-u"]);

    let config = ConfigSet::new();
    let break_only = DiffPostprocessOptions {
        break_score: Some(GIT_DIFF_DEFAULT_BREAK_SCORE),
        rename_threshold: grit_lib::diff::RENAME_DETECTION_DISABLED,
        ..Default::default()
    };
    let git_b = git_raw_lines(
        wt,
        &[
            "diff-index",
            "-r",
            "-B",
            "--cached",
            "--root",
            "--raw",
            &reference.to_hex(),
        ],
    );
    let index = dr.load_index();
    let grit_b = index_to_tree_raw(&dr.repo, &index, &reference, &config, &break_only);
    assert_raw_matches_git(&grit_b, &git_b, "t4008 break without rename");

    let break_rename = DiffPostprocessOptions {
        break_score: Some(GIT_DIFF_DEFAULT_BREAK_SCORE),
        rename_threshold: 50,
        ..Default::default()
    };
    let git_br = git_raw_lines(
        wt,
        &[
            "diff-index",
            "-r",
            "-B",
            "-M50",
            "--cached",
            "--root",
            "--raw",
            &reference.to_hex(),
        ],
    );
    let grit_br = index_to_tree_raw(&dr.repo, &index, &reference, &config, &break_rename);
    assert_raw_matches_git(&grit_br, &git_br, "t4008 break with rename");
}

#[test]
fn t4023_typechange_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("f"), "blob\n").unwrap();
    git_quiet(wt, &["add", "f"]);
    git_quiet(wt, &["commit", "-qm", "blob"]);
    let parent = rev_parse_tree(wt, "HEAD^{tree}");

    fs::remove_file(wt.join("f")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("target", wt.join("f")).unwrap();
    #[cfg(not(unix))]
    {
        eprintln!("SKIP: t4023_typechange requires unix symlinks");
        return;
    }
    git_quiet(wt, &["add", "f"]);
    let commit: ObjectId = git_quiet(wt, &["write-tree"]).trim().parse().unwrap();

    let config = ConfigSet::new();
    let opts = default_opts();
    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--raw",
            &parent.to_hex(),
            &commit.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&parent), Some(&commit), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4023 typechange");
}

#[test]
fn t4054_bogus_tree_typed_error() {
    let dr = DiffRepo::init();
    git_quiet(&dr.worktree, &["commit", "--allow-empty", "-qm", "empty"]);
    let bogus = write_bogus_tree(&dr.repo.odb());
    let good = rev_parse_tree(&dr.worktree, "HEAD^{tree}");
    let err = diff_trees(&dr.repo.odb(), Some(&good), Some(&bogus), "").unwrap_err();
    assert!(
        matches!(err, Error::CorruptObject(_)),
        "expected CorruptObject, got {err:?}"
    );
}

#[test]
fn rename_limit_wide_tree_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    const N: usize = 1001;
    for i in 0..N {
        fs::write(wt.join(format!("old{i}.txt")), format!("payload {i}\n")).unwrap();
    }
    git_quiet(wt, &["add", "."]);
    git_quiet(wt, &["commit", "-qm", "many"]);

    for i in 0..N {
        fs::remove_file(wt.join(format!("old{i}.txt"))).unwrap();
        fs::write(wt.join(format!("new{i}.txt")), format!("payload {i}\n")).unwrap();
    }
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "renamed"]);

    let parent = rev_parse_tree(wt, "HEAD~1^{tree}");
    let commit = rev_parse_tree(wt, "HEAD^{tree}");
    let config = ConfigSet::new();
    let opts = rename_opts(50);

    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "-M50",
            "--root",
            "--raw",
            &parent.to_hex(),
            &commit.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&parent), Some(&commit), &config, &opts);
    assert_eq!(git_lines.len(), N);
    assert_raw_matches_git(&grit_lines, &git_lines, "renameLimit wide tree");
}

#[test]
fn t4011_symlink_tree_raw_matches_git() {
    #[cfg(not(unix))]
    {
        eprintln!("SKIP: t4011 requires unix symlinks");
        return;
    }
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    std::fs::write(wt.join("target"), "tgt\n").unwrap();
    std::os::unix::fs::symlink("target", wt.join("link")).unwrap();
    git_quiet(wt, &["add", "link", "target"]);
    git_quiet(wt, &["commit", "-qm", "symlink"]);
    let parent = rev_parse_tree(wt, "HEAD^{tree}");

    std::fs::remove_file(wt.join("link")).unwrap();
    std::os::unix::fs::symlink("target", wt.join("link2")).unwrap();
    git_quiet(wt, &["add", "-A"]);
    let child: ObjectId = git_quiet(wt, &["write-tree"]).trim().parse().unwrap();

    let config = ConfigSet::new();
    let opts = default_opts();
    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--raw",
            &parent.to_hex(),
            &child.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&parent), Some(&child), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4011 symlink");
}

#[test]
fn rename_limit_config_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    const N: usize = 30;
    for i in 0..N {
        std::fs::write(wt.join(format!("o{i}.txt")), format!("body {i}\n")).unwrap();
    }
    git_quiet(wt, &["add", "."]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    for i in 0..N {
        std::fs::remove_file(wt.join(format!("o{i}.txt"))).unwrap();
        std::fs::write(wt.join(format!("n{i}.txt")), format!("body {i}\n")).unwrap();
    }
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "renamed"]);

    let parent = rev_parse_tree(wt, "HEAD~1^{tree}");
    let commit = rev_parse_tree(wt, "HEAD^{tree}");
    let config = config_with_kv("diff", "renameLimit", "5");
    let opts = rename_opts(50);

    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "-M50",
            "--root",
            "--raw",
            &parent.to_hex(),
            &commit.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&parent), Some(&commit), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "diff.renameLimit");
}

#[test]
fn t4000_root_tree_diff_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("first"), "x\n").unwrap();
    git_quiet(wt, &["add", "first"]);
    let tree: ObjectId = git_quiet(wt, &["write-tree"]).trim().parse().unwrap();
    let config = ConfigSet::new();
    let opts = default_opts();
    let empty_tree: ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".parse().unwrap();
    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--raw",
            &empty_tree.to_hex(),
            &tree.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&empty_tree), Some(&tree), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4000 root tree");
}

#[test]
fn diff_tree_to_worktree_and_show_tree_entries() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("tracked"), "v1\n").unwrap();
    git_quiet(wt, &["add", "tracked"]);
    git_quiet(wt, &["commit", "-qm", "c1"]);
    let tree = rev_parse_tree(wt, "HEAD^{tree}");
    fs::write(wt.join("tracked"), "v2\n").unwrap();
    fs::write(wt.join("new.txt"), "n\n").unwrap();
    git_quiet(wt, &["add", "new.txt"]);

    let index = dr.load_index();
    let config = ConfigSet::new();
    let opts = default_opts();
    let git_lines = git_raw_lines(wt, &["diff", "-r", "--raw", &format!("{}:", tree.to_hex())]);
    let raw = grit_lib::diff::diff_tree_to_worktree(&dr.repo.odb(), Some(&tree), wt, &index)
        .expect("tree_to_worktree");
    let wctx = grit_lib::diff::DiffPostprocessWorktree {
        repository_git_dir: &dr.repo.git_dir(),
        index: &index,
    };
    let entries = support::postprocess(
        &dr.repo.odb(),
        raw,
        Some(&tree),
        Some(wt),
        Some(&wctx),
        &config,
        &opts,
    );
    let grit_lines = grit_raw_lines_abbrev(&entries, 7);
    assert_raw_matches_git(&grit_lines, &git_lines, "tree↔worktree");

    let show = diff_trees_show_tree_entries(&dr.repo.odb(), Some(&tree), Some(&tree), "")
        .expect("show tree entries");
    assert!(
        show.is_empty(),
        "identical trees produce no show-tree lines"
    );
}

#[test]
fn rename_similarity_trailing_newline_is_add_delete_not_rename_at_m50() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("old"), "a").unwrap();
    git_quiet(wt, &["add", "old"]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    let base: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    fs::write(wt.join("new"), "a\n").unwrap();
    git_quiet(wt, &["add", "new"]);
    git_quiet(wt, &["rm", "old"]);
    git_quiet(wt, &["commit", "-qm", "newline-only change"]);
    let tip: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let config = ConfigSet::new();
    let opts = rename_opts(50);
    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "-M50",
            "--root",
            "--raw",
            &base.to_hex(),
            &tip.to_hex(),
        ],
    );
    assert!(
        git_lines.iter().any(|l| l.contains(" A\tnew")),
        "git must report add+delete, not rename: {git_lines:?}"
    );
    assert!(
        git_lines.iter().any(|l| l.contains(" D\told")),
        "git must report add+delete, not rename: {git_lines:?}"
    );
    assert!(
        !git_lines.iter().any(|l| {
            l.split_whitespace()
                .any(|tok| tok.starts_with('R') && tok.len() == 4)
        }),
        "git must not classify as rename: {git_lines:?}"
    );
    let grit_lines = commits_diff_raw(&dr.repo, &base, &tip, &config, &opts);
    assert_raw_matches_git(
        &grit_lines,
        &git_lines,
        "trailing newline only change must not pair as rename at -M50",
    );
}

#[test]
fn diff_commits_rename_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("a"), "content\n").unwrap();
    git_quiet(wt, &["add", "a"]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    let base: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    git_quiet(wt, &["mv", "a", "b"]);
    git_quiet(wt, &["commit", "-qm", "rename"]);
    let tip: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let config = ConfigSet::new();
    let opts = rename_opts(50);
    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "-M50",
            "--root",
            "--raw",
            &base.to_hex(),
            &tip.to_hex(),
        ],
    );
    let grit_lines = commits_diff_raw(&dr.repo, &base, &tip, &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "diff_commits rename");

    let parent_tree = rev_parse_tree(wt, "HEAD~1^{tree}");
    let commit_tree = rev_parse_tree(wt, "HEAD^{tree}");
    let with_renames = diff_trees_with_renames(
        &dr.repo.odb(),
        Some(&parent_tree),
        Some(&commit_tree),
        "",
        50,
    )
    .expect("with_renames");
    assert!(
        with_renames
            .iter()
            .any(|e| e.status == grit_lib::diff::DiffStatus::Renamed),
        "diff_trees_with_renames must detect rename"
    );
}

#[test]
fn t4043_binary_rename_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let bin = (0u8..128).collect::<Vec<_>>();
    fs::write(wt.join("bin"), &bin).unwrap();
    git_quiet(wt, &["add", "bin"]);
    git_quiet(wt, &["commit", "-qm", "bin"]);
    let parent = rev_parse_tree(wt, "HEAD^{tree}");
    git_quiet(wt, &["mv", "bin", "renamed.bin"]);
    git_quiet(wt, &["commit", "-qm", "mv"]);
    let commit = rev_parse_tree(wt, "HEAD^{tree}");
    let config = ConfigSet::new();
    let opts = rename_opts(50);
    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "-M50",
            "--root",
            "--raw",
            &parent.to_hex(),
            &commit.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&parent), Some(&commit), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4043 binary rename");
    let abbrev = grit_raw_lines_abbrev(
        &support::postprocess(
            &dr.repo.odb(),
            diff_trees(&dr.repo.odb(), Some(&parent), Some(&commit), "").expect("trees"),
            Some(&parent),
            None,
            None,
            &config,
            &opts,
        ),
        7,
    );
    assert_eq!(abbrev.len(), git_lines.len());
}

#[test]
fn t4006_diff_mode_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::write(wt.join("x"), "v1\n").unwrap();
    git_quiet(wt, &["add", "x"]);
    git_quiet(wt, &["commit", "-qm", "c1"]);
    let old = rev_parse_tree(wt, "HEAD^{tree}");
    fs::write(wt.join("x"), "v2\n").unwrap();
    git_quiet(wt, &["add", "x"]);
    let new = git_quiet(wt, &["write-tree"])
        .trim()
        .parse::<ObjectId>()
        .unwrap();
    let config = ConfigSet::new();
    let opts = default_opts();
    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--raw",
            &old.to_hex(),
            &new.to_hex(),
        ],
    );
    let grit_lines = tree_diff_raw(&dr.repo, Some(&old), Some(&new), &config, &opts);
    assert_raw_matches_git(&grit_lines, &git_lines, "t4006 mode");
}
