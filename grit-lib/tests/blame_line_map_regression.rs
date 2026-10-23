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
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    let result = blame_file(&repo, "add.c", &BlameOptions::default()).unwrap();
    let git_out = Command::new("git")
        .current_dir(root)
        .args(["blame", "--porcelain", "add.c"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(git_out.status.success());
    let mut git_by_line = std::collections::HashMap::new();
    let mut commit = String::new();
    let mut final_line = 0usize;
    for line in String::from_utf8_lossy(&git_out.stdout).lines() {
        if let Some(rest) = line.strip_prefix('\t') {
            let _ = rest;
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
    for line in &result.lines {
        assert_eq!(
            line.commit.to_hex(),
            git_by_line[&line.final_line],
            "line {}",
            line.final_line
        );
    }
}
