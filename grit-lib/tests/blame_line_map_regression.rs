//! Regression for blame line maps on moved includes (#1610).

use grit_lib::blame::{build_line_map, BlameDiffAlgorithm};

#[test]
fn moved_include_line_maps_as_insert_not_reused_parent_index() {
    let old = [
        "",
        "#include \"cache.h\"",
        "#include \"builtin.h\"",
        "#include \"advice.h\"",
        "#include \"pathspec.h\"",
        "static int chmod_pathspec(struct pathspec_item *item, void *cb)",
        "{",
        "    return 0;",
        "}",
        "#include \"run-command.h\"",
        "#include \"builtin.h\"",
    ];
    let new = [
        "",
        "#include \"builtin.h\"",
        "#include \"advice.h\"",
        "#include \"pathspec.h\"",
        "#include \"run-command.h\"",
        "static int chmod_pathspec(struct pathspec_item *item, void *cb)",
        "{",
        "    return 0;",
        "}",
    ];
    let map = build_line_map(&old, &new, BlameDiffAlgorithm::Myers, true);
    assert_eq!(
        map[4], None,
        "moved run-command.h is an insert at the new hunk (git blame stops on shuffle commit): {:?}",
        map
    );
}

use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use std::path::Path;
use std::process::Command;

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t.com");
}

fn git_run(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    git_env(&mut cmd);
    assert!(cmd.output().unwrap().status.success(), "git {:?}", args);
}

#[test]
fn blame_merge_moved_include_matches_git() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.email", "t@t.com"]);
    git_run(root, &["config", "user.name", "T"]);
    std::fs::write(
        root.join("add.c"),
        "\n#include \"cache.h\"\n#include \"builtin.h\"\n#include \"advice.h\"\nstatic int chmod_pathspec(struct pathspec_item *item, void *cb)\n{\n    return 0;\n}\n",
    )
    .unwrap();
    git_run(root, &["add", "add.c"]);
    git_run(root, &["commit", "-q", "-m", "ancient"]);
    git_run(root, &["switch", "-c", "side"]);
    std::fs::write(
        root.join("add.c"),
        "\n#include \"cache.h\"\n#include \"builtin.h\"\n#include \"advice.h\"\nstatic int chmod_pathspec(struct pathspec_item *item, void *cb)\n{\n    return 0;\n}\n#include \"run-command.h\"\n#include \"builtin.h\"\n",
    )
    .unwrap();
    git_run(root, &["commit", "-q", "-am", "side"]);
    git_run(root, &["switch", "main"]);
    std::fs::write(
        root.join("add.c"),
        "\n#include \"cache.h\"\n#include \"builtin.h\"\n#include \"advice.h\"\n#include \"pathspec.h\"\nstatic int chmod_pathspec(struct pathspec_item *item, void *cb)\n{\n    return 0;\n}\n",
    )
    .unwrap();
    git_run(root, &["commit", "-q", "-am", "mainline"]);
    git_run(root, &["merge", "--no-ff", "side", "-q", "-m", "merge"]);
    std::fs::write(
        root.join("add.c"),
        "\n#include \"builtin.h\"\n#include \"advice.h\"\n#include \"pathspec.h\"\n#include \"run-command.h\"\nstatic int chmod_pathspec(struct pathspec_item *item, void *cb)\n{\n    return 0;\n}\n",
    )
    .unwrap();
    git_run(root, &["commit", "-q", "-am", "shuffle"]);
    assert_blame_matches_git(root, "add.c");
}

/// Assert `blame_file` attributes every line of `path` to the commit `git blame` reports.
fn assert_blame_matches_git(root: &Path, path: &str) {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    let result = blame_file(&repo, path, &BlameOptions::default()).unwrap();
    let mut cmd = Command::new("git");
    cmd.current_dir(root).args(["blame", "--porcelain", path]);
    git_env(&mut cmd);
    let git_out = cmd.output().unwrap();
    assert!(git_out.status.success());
    let mut git_by_line = std::collections::HashMap::new();
    let mut commit = String::new();
    let mut final_line = 0usize;
    for line in String::from_utf8_lossy(&git_out.stdout).lines() {
        if line.starts_with('\t') {
            git_by_line.insert(final_line, commit.clone());
            continue;
        }
        let mut parts = line.split_whitespace();
        if let Some(sha) = parts.next() {
            if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                commit = sha.to_string();
                let _ = parts.next();
                if let Some(fl) = parts.next() {
                    final_line = fl.parse().unwrap();
                }
            }
        }
    }
    assert_eq!(result.lines.len(), git_by_line.len());
    for line in &result.lines {
        assert_eq!(
            line.commit.to_hex(),
            git_by_line[&line.final_line],
            "{path} line {}",
            line.final_line
        );
    }
}

/// Commit each version of `f` in turn, with `diff.indentHeuristic` set as given.
fn commit_versions(root: &Path, indent_heuristic: bool, versions: &[&str]) {
    git_run(root, &["init", "-q", "-b", "main"]);
    let heuristic = if indent_heuristic { "true" } else { "false" };
    git_run(root, &["config", "diff.indentHeuristic", heuristic]);
    for (i, text) in versions.iter().enumerate() {
        std::fs::write(root.join("f"), text).unwrap();
        git_run(root, &["add", "f"]);
        git_run(root, &["commit", "-q", "-m", &format!("v{}", i + 1)]);
    }
}

fn assert_versions_blame_like_git(versions: &[&str]) {
    for indent_heuristic in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        commit_versions(tmp.path(), indent_heuristic, versions);
        assert_blame_matches_git(tmp.path(), "f");
    }
}

/// #2108: an insertion that can slide into the unchanged suffix lands where git puts it,
/// even when another hunk sits a few lines above.
#[test]
fn inserted_block_slides_into_suffix_like_git() {
    assert_versions_blame_like_git(&[
        "PROG=a b c\n\t$(CC) -o cat-file\n\nread-cache.o: cache.h\n",
        "PROG=a b c fsck-cache\n\t$(CC) -o cat-file\n\nfsck-cache: fsck-cache.o\n\t$(CC) -o fsck-cache\n\nread-cache.o: cache.h\n",
    ]);
}

/// The same insertion without the first-line edit is a single hunk.
#[test]
fn single_insertion_hunk_matches_git() {
    assert_versions_blame_like_git(&[
        "PROG=a b c\n\t$(CC) -o cat-file\n\nread-cache.o: cache.h\n",
        "PROG=a b c\n\t$(CC) -o cat-file\n\nfsck-cache: fsck-cache.o\n\t$(CC) -o fsck-cache\n\nread-cache.o: cache.h\n",
    ]);
}

/// A duplicated `#` line at the edge of the unchanged suffix belongs to the inserted block.
#[test]
fn duplicate_comment_boundary_matches_git() {
    assert_versions_blame_like_git(&["x = 1\n#\ny = 2\n", "x = 10\n#\nz = 3\n#\ny = 2\n"]);
    assert_versions_blame_like_git(&[
        "#\n# top\n#\nbody\nend\n",
        "#\n# top\n#\nnew\n#\nbody\nEND\n",
    ]);
}

/// Git drops the common tail of a blame diff in 1 KiB blocks, so a one-line insertion into a
/// long run of identical lines stops where that cut leaves the file.
#[test]
fn insertion_into_long_duplicate_run_stops_at_git_tail_cut() {
    let old = format!("a\n{}", "b\n".repeat(1000));
    let new = format!("a\n{}", "b\n".repeat(1001));
    assert_versions_blame_like_git(&[&old, &new]);
}

/// Xdiff classifies lines by how often they occur in the whole file: a `}` between unique
/// lines is part of the change because `}` is common far away from it.
#[test]
fn frequent_line_inside_rewrite_is_blamed_on_the_rewrite() {
    let version = |middle: &[&str]| {
        let mut text = "}\n".repeat(60);
        text.extend((0..30).map(|i| format!("head {i}\n")));
        for line in middle {
            text.push_str(line);
            text.push('\n');
        }
        text.extend((0..30).map(|i| format!("tail {i}\n")));
        text
    };
    let old = version(&["u1", "u2", "u3", "u4", "}", "u5", "u6", "u7"]);
    let new = version(&["v1", "v2", "v3", "v4", "}", "v5", "v6", "v7"]);
    assert_versions_blame_like_git(&[&old, &new]);
}
