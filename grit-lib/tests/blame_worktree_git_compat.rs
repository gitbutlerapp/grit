//! `blame_file` on uncommitted content (worktree file or caller buffer) against system
//! `git blame`, which blames such content as a fake commit on top of `HEAD`.

use std::path::Path;

use grit_lib::blame::{
    blame_file, resolve_blame_line_range_with_options, BlameFinalSource, BlameOptions, BlameResult,
    CopyDetection,
};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::WhitespaceMode;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd, rev_parse};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attribution {
    final_line: usize,
    commit: ObjectId,
    original_line: usize,
}

fn parse_porcelain(output: &str) -> Vec<Attribution> {
    let mut lines = Vec::new();
    let mut group: Option<(ObjectId, usize, usize)> = None;
    for raw in output.lines() {
        if raw.starts_with('\t') {
            if let Some((commit, orig, final_line)) = group {
                lines.push(Attribution {
                    final_line,
                    commit,
                    original_line: orig,
                });
                group = Some((commit, orig + 1, final_line + 1));
            }
            continue;
        }
        let mut parts = raw.split_whitespace();
        let (Some(sha), Some(orig), Some(final_line)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !matches!(sha.len(), 40 | 64) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let (Ok(commit), Ok(orig), Ok(final_line)) = (
            ObjectId::from_hex(sha),
            orig.parse::<usize>(),
            final_line.parse::<usize>(),
        ) else {
            continue;
        };
        group = Some((commit, orig, final_line));
    }
    lines.sort_by_key(|l| l.final_line);
    lines
}

fn git_blame(dir: &Path, args: &[&str]) -> Vec<Attribution> {
    let mut argv = vec!["blame", "--porcelain"];
    argv.extend_from_slice(args);
    parse_porcelain(&git(dir, &argv))
}

fn attributions(result: &BlameResult) -> Vec<Attribution> {
    result
        .lines
        .iter()
        .map(|line| Attribution {
            final_line: line.final_line,
            commit: line.commit,
            original_line: line.original_line,
        })
        .collect()
}

fn open_repo(dir: &Path) -> Repository {
    Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir),
    )
    .expect("open repo")
}

fn worktree_options() -> BlameOptions {
    BlameOptions {
        final_source: BlameFinalSource::Worktree,
        ..BlameOptions::default()
    }
}

fn blame(repo: &Repository, path: &str, options: &BlameOptions) -> BlameResult {
    blame_file(repo, path, options).expect("blame_file")
}

/// Three commits touching `f.txt`, so attributions span several commits.
fn history(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "first"]);
    std::fs::write(root.join("f.txt"), "one\ntwo\nTHREE\nfour\nfive\nsix\n").unwrap();
    git(root, &["commit", "-qam", "second"]);
    std::fs::write(
        root.join("f.txt"),
        "zero\none\ntwo\nTHREE\nfour\nfive\nsix\n",
    )
    .unwrap();
    git(root, &["commit", "-qam", "third"]);
}

fn assert_worktree_matches_git(root: &Path, path: &str) -> BlameResult {
    let repo = open_repo(root);
    let result = blame(&repo, path, &worktree_options());
    assert_eq!(attributions(&result), git_blame(root, &["--", path]));
    for line in &result.lines {
        assert_eq!(line.not_committed, line.commit.is_zero(), "{line:?}");
    }
    result
}

#[test]
fn worktree_edits_match_git() {
    let edits: [(&str, &str, usize); 4] = [
        (
            "insert at top",
            "// edited\nzero\none\ntwo\nTHREE\nfour\nfive\nsix\n",
            1,
        ),
        ("delete lines", "zero\ntwo\nfour\nfive\nsix\n", 0),
        (
            "edit middle",
            "zero\none\ntwo\nthree again\nfour\nfive\nsix\n",
            1,
        ),
        (
            "mixed",
            "zero\nnew a\none\ntwo\nTHREE\nnew b\nfive\nsix\nseven",
            3,
        ),
    ];
    for (label, text, uncommitted) in edits {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        history(root);
        std::fs::write(root.join("f.txt"), text).unwrap();
        let result = assert_worktree_matches_git(root, "f.txt");
        assert_eq!(
            result
                .lines
                .iter()
                .filter(|line| line.not_committed)
                .count(),
            uncommitted,
            "{label}"
        );
        let contents: Vec<&str> = result.lines.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(contents, text.lines().collect::<Vec<_>>(), "{label}");
    }
}

#[test]
fn worktree_insert_shifts_following_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    let head = rev_parse(root, "HEAD");
    std::fs::write(
        root.join("f.txt"),
        "// edited\nzero\none\ntwo\nTHREE\nfour\nfive\nsix\n",
    )
    .unwrap();
    let repo = open_repo(root);
    let result = blame(&repo, "f.txt", &worktree_options());
    let first = &result.lines[0];
    assert!(first.not_committed);
    assert_eq!(first.commit, ObjectId::zero());
    assert_eq!((first.final_line, first.original_line), (1, 1));
    assert_eq!(first.content, "// edited");
    let second = &result.lines[1];
    assert!(!second.not_committed);
    assert_eq!((second.commit, second.final_line), (head, 2));
    assert_eq!(second.original_line, 1);
}

#[test]
fn clean_worktree_matches_revision_blame() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    let repo = open_repo(root);
    let worktree = blame(&repo, "f.txt", &worktree_options());
    let revision = blame(&repo, "f.txt", &BlameOptions::default());
    assert_eq!(worktree, revision);
    assert!(worktree.lines.iter().all(|line| !line.not_committed));
}

#[test]
fn contents_buffer_matches_git_contents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    // The worktree has its own edit; `--contents` must ignore it.
    std::fs::write(root.join("f.txt"), "worktree only\n").unwrap();
    let buffer = "zero\none\nunsaved\ntwo\nTHREE\nfour\nsix\n";
    let buffer_path = root.join("buffer.txt");
    std::fs::write(&buffer_path, buffer).unwrap();
    let buffer_arg = buffer_path.to_string_lossy().into_owned();

    let repo = open_repo(root);
    let result = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            final_source: BlameFinalSource::Contents(buffer.as_bytes().to_vec()),
            ..BlameOptions::default()
        },
    );
    assert_eq!(
        attributions(&result),
        git_blame(root, &["--contents", &buffer_arg, "--", "f.txt"])
    );
    assert_eq!(
        result
            .lines
            .iter()
            .filter(|line| line.not_committed)
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>(),
        ["unsaved"]
    );

    let older = rev_parse(root, "HEAD~1");
    let on_older = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            start: Some(older),
            final_source: BlameFinalSource::Contents(buffer.as_bytes().to_vec()),
            ..BlameOptions::default()
        },
    );
    assert_eq!(
        attributions(&on_older),
        git_blame(root, &["--contents", &buffer_arg, "HEAD~1", "--", "f.txt"])
    );
}

#[test]
fn revision_source_ignores_dirty_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(root.join("f.txt"), "dirty\nzero\none\n").unwrap();
    let repo = open_repo(root);
    let head = rev_parse(root, "HEAD");
    let explicit = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            start: Some(head),
            ..BlameOptions::default()
        },
    );
    assert_eq!(
        attributions(&explicit),
        git_blame(root, &["HEAD", "--", "f.txt"])
    );
    assert_eq!(explicit, blame(&repo, "f.txt", &BlameOptions::default()));
    assert!(explicit.lines.iter().all(|line| !line.not_committed));
}

#[test]
fn crlf_worktree_is_cleaned_before_diffing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    git(root, &["config", "core.autocrlf", "true"]);
    let repo = open_repo(root);

    // Only line endings differ: nothing is uncommitted.
    std::fs::write(
        root.join("f.txt"),
        "zero\r\none\r\ntwo\r\nTHREE\r\nfour\r\nfive\r\nsix\r\n",
    )
    .unwrap();
    let result = assert_worktree_matches_git(root, "f.txt");
    assert!(result.lines.iter().all(|line| !line.not_committed));
    assert_eq!(result, blame(&repo, "f.txt", &BlameOptions::default()));

    std::fs::write(
        root.join("f.txt"),
        "zero\r\none\r\nadded\r\ntwo\r\nTHREE\r\nfour\r\nfive\r\nsix\r\n",
    )
    .unwrap();
    let result = assert_worktree_matches_git(root, "f.txt");
    assert_eq!(
        result
            .lines
            .iter()
            .filter(|line| line.not_committed)
            .count(),
        1
    );
}

#[test]
fn untracked_file_is_not_in_head() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(root.join("untracked.txt"), "hello\n").unwrap();
    let out = git_cmd(&["blame", "--", "untracked.txt"])
        .in_dir(root)
        .exec();
    assert!(!out.ok(), "git blames untracked files: {}", out.dump("git"));

    let repo = open_repo(root);
    let err = blame_file(&repo, "untracked.txt", &worktree_options()).unwrap_err();
    assert!(
        matches!(&err, Error::RevParse(_)),
        "unexpected error: {err:?}"
    );
    assert!(err.to_string().contains("untracked.txt"), "{err}");
}

#[test]
fn missing_worktree_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::remove_file(root.join("f.txt")).unwrap();
    let repo = open_repo(root);
    assert!(blame_file(&repo, "f.txt", &worktree_options()).is_err());
    // The committed file can still be blamed.
    assert_eq!(
        blame(&repo, "f.txt", &BlameOptions::default()).lines.len(),
        7
    );
}

#[test]
fn staged_new_file_is_entirely_uncommitted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(root.join("new.txt"), "alpha\nbeta\n").unwrap();
    git(root, &["add", "new.txt"]);
    let result = assert_worktree_matches_git(root, "new.txt");
    assert_eq!(result.lines.len(), 2);
    assert!(result.lines.iter().all(|line| line.not_committed));
}

#[test]
fn merge_head_lines_come_from_the_incoming_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(
        root.join("f.txt"),
        "zero\none\ntwo\nTHREE\nfour\nfive\nsix\nfrom side\n",
    )
    .unwrap();
    git(root, &["commit", "-qam", "side"]);
    let side = rev_parse(root, "HEAD");
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(
        root.join("f.txt"),
        "from main\nzero\none\ntwo\nTHREE\nfour\nfive\nsix\n",
    )
    .unwrap();
    git(root, &["commit", "-qam", "main"]);
    git(root, &["merge", "-q", "--no-commit", "side"]);
    assert!(root.join(".git/MERGE_HEAD").exists());
    // An extra local line on top of the merge result.
    let merged = std::fs::read_to_string(root.join("f.txt")).unwrap();
    std::fs::write(root.join("f.txt"), format!("{merged}local\n")).unwrap();

    let result = assert_worktree_matches_git(root, "f.txt");
    let from_side = result
        .lines
        .iter()
        .find(|line| line.content == "from side")
        .unwrap();
    assert_eq!(from_side.commit, side);
    assert!(result.lines.last().unwrap().not_committed);
}

#[test]
fn first_parent_ignores_merge_head_for_uncommitted_content() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f.txt"), "base\n").unwrap();
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("f.txt"), "base\nside\n").unwrap();
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("f.txt"), "main\nbase\n").unwrap();
    git(root, &["commit", "-qam", "main"]);
    git(root, &["merge", "-q", "--no-commit", "side"]);
    let merged = std::fs::read_to_string(root.join("f.txt")).unwrap();
    std::fs::write(root.join("f.txt"), format!("{merged}local\n")).unwrap();

    let repo = open_repo(root);
    let result = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            first_parent_only: true,
            ..worktree_options()
        },
    );
    assert_eq!(
        attributions(&result),
        git_blame(root, &["--first-parent", "--", "f.txt"])
    );
    let uncommitted: Vec<&str> = result
        .lines
        .iter()
        .filter(|line| line.not_committed)
        .map(|line| line.content.as_str())
        .collect();
    assert_eq!(uncommitted, ["side", "local"]);

    // Without --first-parent the incoming branch still owns its line.
    let side = rev_parse(root, "side");
    let all_parents = assert_worktree_matches_git(root, "f.txt");
    let side_line = all_parents
        .lines
        .iter()
        .find(|line| line.content == "side")
        .unwrap();
    assert_eq!(side_line.commit, side);
}

#[test]
fn uncommitted_blame_writes_no_objects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(root.join("f.txt"), "brand new content\nzero\n").unwrap();
    let before = git(root, &["count-objects", "-v"]);
    let repo = open_repo(root);
    blame(&repo, "f.txt", &worktree_options());
    blame(
        &repo,
        "f.txt",
        &BlameOptions {
            final_source: BlameFinalSource::Contents(b"another buffer\n".to_vec()),
            ..BlameOptions::default()
        },
    );
    assert_eq!(git(root, &["count-objects", "-v"]), before);
    git(root, &["fsck", "--strict"]);
}

#[test]
fn uncommitted_commit_info_carries_the_given_time() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(root.join("f.txt"), "new\nzero\n").unwrap();
    let repo = open_repo(root);
    let now = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let result = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            now: Some(now),
            ..worktree_options()
        },
    );
    let info = &result.commits[&ObjectId::zero()];
    assert_eq!(info.author_time, 1_800_000_000);
    assert_eq!(
        info.author,
        "Not Committed Yet <not.committed.yet> 1800000000 +0000"
    );
    assert_eq!(info.summary, "Not Committed Yet");
    let without_time = blame(&repo, "f.txt", &worktree_options());
    assert_eq!(without_time.commits[&ObjectId::zero()].author_time, 0);
    let hunks = result.hunks();
    assert!(hunks[0].commit.is_zero() && hunks[0].not_committed);
    assert_eq!((hunks[0].final_start, hunks[0].count), (1, 1));
    assert!(hunks[0].lines(&result)[0].not_committed);
    assert!(!hunks[1].not_committed);
}

#[test]
fn contents_commit_info_uses_the_external_file_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    let repo = open_repo(root);
    let result = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            final_source: BlameFinalSource::Contents(b"buffer only\n".to_vec()),
            ..worktree_options()
        },
    );
    let info = &result.commits[&ObjectId::zero()];
    assert_eq!(
        info.author,
        "External file (--contents) <not.committed.yet> 0 +0000"
    );
    assert_eq!(info.summary, "External file (--contents)");
}

/// `author` of the null-commit group in `git blame --porcelain` output.
fn git_uncommitted_author(dir: &Path, args: &[&str]) -> String {
    let mut argv = vec!["blame", "--porcelain"];
    argv.extend_from_slice(args);
    let out = git(dir, &argv);
    let mut in_null_group = false;
    for raw in out.lines() {
        if let Some((sha, _)) = raw.split_once(' ') {
            if matches!(sha.len(), 40 | 64) && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                in_null_group = sha.chars().all(|c| c == '0');
                continue;
            }
        }
        if let Some(name) = raw.strip_prefix("author ").filter(|_| in_null_group) {
            return name.to_owned();
        }
    }
    panic!("no uncommitted group in git blame output:\n{out}");
}

#[test]
fn uncommitted_author_name_matches_git_porcelain() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(root.join("f.txt"), "new\nzero\n").unwrap();
    let buffer = b"zero\nbuffer only\n";
    std::fs::write(root.join("buffer.txt"), buffer).unwrap();
    let repo = open_repo(root);

    let mut worktree = blame(&repo, "f.txt", &worktree_options());
    worktree.name_uncommitted_author(&BlameFinalSource::Worktree);
    let name = BlameFinalSource::Worktree.uncommitted_author_name();
    assert_eq!(name, git_uncommitted_author(root, &["--", "f.txt"]));
    let info = &worktree.commits[&ObjectId::zero()];
    assert!(info
        .author
        .starts_with(&format!("{name} <not.committed.yet> ")));
    assert_eq!(info.summary, name);

    let source = BlameFinalSource::Contents(buffer.to_vec());
    let mut contents = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            final_source: source.clone(),
            ..BlameOptions::default()
        },
    );
    contents.name_uncommitted_author(&source);
    assert_eq!(
        source.uncommitted_author_name(),
        git_uncommitted_author(root, &["--contents", "buffer.txt", "--", "f.txt"])
    );
    assert_eq!(
        contents.commits[&ObjectId::zero()].summary,
        source.uncommitted_author_name()
    );
    let hunks = contents.hunks();
    let uncommitted: Vec<_> = hunks.iter().filter(|h| h.not_committed).collect();
    assert_eq!(uncommitted.len(), 1);
    let lines = contents.hunk_lines(uncommitted[0]);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].content, "buffer only");
}

#[test]
fn line_range_resolves_against_the_final_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(
        root.join("f.txt"),
        "a\nb\nc\nzero\none\ntwo\nTHREE\nfour\nfive\nsix\n",
    )
    .unwrap();
    let repo = open_repo(root);
    let mut options = worktree_options();
    let range = resolve_blame_line_range_with_options(&repo, "f.txt", "/^two$/,+2", &options)
        .expect("range");
    assert_eq!(range, 6..=7);
    options.line_range = Some(range);
    let result = blame(&repo, "f.txt", &options);
    assert_eq!(
        attributions(&result),
        git_blame(root, &["-L", "/^two$/,+2", "--", "f.txt"])
    );
    let numeric = resolve_blame_line_range_with_options(&repo, "f.txt", "9,10", &options)
        .expect("numeric range");
    assert_eq!(numeric, 9..=10);
}

#[test]
fn ignore_whitespace_applies_to_uncommitted_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(
        root.join("f.txt"),
        "zero\n  one\ntwo   \nTHREE\nfour\nfive\nsix\n",
    )
    .unwrap();
    let repo = open_repo(root);
    let result = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            whitespace: WhitespaceMode::ignore_all_space(),
            ..worktree_options()
        },
    );
    assert_eq!(
        attributions(&result),
        git_blame(root, &["-w", "--", "f.txt"])
    );
    assert!(result.lines.iter().all(|line| !line.not_committed));
}

#[test]
fn copy_detection_finds_uncommitted_lines_in_other_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::fs::write(
        root.join("other.txt"),
        "this is a fairly long line copied from another file ok\nshort\n",
    )
    .unwrap();
    git(root, &["add", "other.txt"]);
    git(root, &["commit", "-qm", "other"]);
    let other = rev_parse(root, "HEAD");
    std::fs::write(
        root.join("f.txt"),
        "zero\none\nthis is a fairly long line copied from another file ok\ntwo\nTHREE\nfour\nfive\nsix\n",
    )
    .unwrap();
    let repo = open_repo(root);
    let result = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            copy_detection: CopyDetection::FromAnyCommit,
            ..worktree_options()
        },
    );
    assert_eq!(
        attributions(&result),
        git_blame(root, &["-C", "-C", "-C", "--", "f.txt"])
    );
    assert_eq!(result.lines[2].commit, other);
    assert_eq!(result.lines[2].original_path, "other.txt");
}

#[cfg(unix)]
#[test]
fn symlink_target_is_blamed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    std::os::unix::fs::symlink("f.txt", root.join("link")).unwrap();
    git(root, &["add", "link"]);
    git(root, &["commit", "-qm", "link"]);
    std::fs::remove_file(root.join("link")).unwrap();
    std::os::unix::fs::symlink("elsewhere.txt", root.join("link")).unwrap();
    let result = assert_worktree_matches_git(root, "link");
    assert_eq!(result.lines.len(), 1);
    assert!(result.lines[0].not_committed);
    assert_eq!(result.lines[0].content, "elsewhere.txt");
}

#[test]
fn bare_repository_rejects_worktree_but_accepts_contents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    history(root);
    let bare = dir.path().join("bare.git");
    git(
        root,
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open bare");
    let err = blame_file(&repo, "f.txt", &worktree_options()).unwrap_err();
    assert!(
        matches!(err, Error::BlameWorktreeBareRepository),
        "unexpected error: {err:?}"
    );
    let result = blame(
        &repo,
        "f.txt",
        &BlameOptions {
            final_source: BlameFinalSource::Contents(b"zero\nnew\none\n".to_vec()),
            ..BlameOptions::default()
        },
    );
    let flags: Vec<bool> = result.lines.iter().map(|l| l.not_committed).collect();
    assert_eq!(flags, [false, true, false]);
}

/// Deterministic random edits of a multi-commit file, each compared with `git blame`.
#[test]
fn random_worktree_edits_match_git() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let mut lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    let mut seed: u64 = 0x5eed_2180;
    let mut next = move |bound: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 33) as usize) % bound
    };
    let edit = |lines: &mut Vec<String>, next: &mut dyn FnMut(usize) -> usize, tag: &str| {
        for step in 0..4 {
            let at = next(lines.len() + 1);
            match next(4) {
                0 if !lines.is_empty() && at < lines.len() => {
                    lines.remove(at);
                }
                1 if at < lines.len() => lines[at] = format!("{tag} changed {step}"),
                2 => lines.insert(at, "}".to_owned()),
                _ => lines.insert(at, format!("{tag} added {step}")),
            }
        }
    };
    for commit in 0..4 {
        edit(&mut lines, &mut next, &format!("c{commit}"));
        std::fs::write(root.join("f.txt"), lines.join("\n") + "\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", &format!("commit {commit}")]);
    }
    for round in 0..12 {
        let mut dirty = lines.clone();
        edit(&mut dirty, &mut next, &format!("wt{round}"));
        std::fs::write(root.join("f.txt"), dirty.join("\n") + "\n").unwrap();
        assert_worktree_matches_git(root, "f.txt");
    }
}
