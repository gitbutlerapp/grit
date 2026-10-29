//! `RepositoryStash::diff_stat` against system `git stash show --numstat`.

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{DiffOptions, DiffStat, WhitespaceMode};
use grit_lib::repo::Repository;
use grit_lib::stash::StashDiffOptions;
use grit_test_support::git;

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

/// A stash with text edits, an added EOF newline, a line-ending-only change, a binary edit,
/// a whitespace-only edit, a mode-only change, a deletion, a staged addition and an
/// untracked file.
fn stash_fixture(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    let write = |name: &str, data: &[u8]| std::fs::write(root.join(name), data).expect("write");
    write("text.txt", b"one\ntwo\nthree\n");
    write("noeol.txt", b"a\nb");
    write("crlf.txt", b"x\ny\n");
    write("bin.dat", b"bin\0one");
    write("ws.txt", b"a b\nc\n");
    write("mode.sh", b"echo hi\n");
    write("gone.txt", b"bye\nnow\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);

    write("text.txt", b"one\n2\nthree\nfour\n");
    write("noeol.txt", b"a\nb\n");
    write("crlf.txt", b"x\r\ny\n");
    write("bin.dat", b"bin\0two");
    write("ws.txt", b"a  b\nc\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("mode.sh"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    std::fs::remove_file(root.join("gone.txt")).expect("remove");
    write("added.txt", b"new\nfile\n");
    git(root, &["add", "added.txt"]);
    write("aa-untracked.txt", b"loose\n");
    git(root, &["stash", "push", "-q", "-u"]);
}

/// `--numstat` rows as grit renders them: `-` counts for binary files.
fn numstat_rows(stat: &DiffStat) -> Vec<String> {
    stat.files()
        .iter()
        .map(|file| {
            if file.binary {
                format!("-\t-\t{}", file.path)
            } else {
                format!(
                    "{}\t{}\t{}",
                    file.lines.insertions, file.lines.deletions, file.path
                )
            }
        })
        .collect()
}

fn git_numstat(root: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["stash", "show", "--numstat", "--no-renames"];
    args.extend_from_slice(extra);
    args.push("stash@{0}");
    git(root, &args).lines().map(str::to_owned).collect()
}

#[test]
fn stash_diff_stat_matches_git_numstat() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_fixture(dir.path());
    let repo = open_grit(dir.path());

    for include_untracked in [false, true] {
        let stat = repo
            .stash()
            .diff_stat(
                0,
                &StashDiffOptions { include_untracked },
                DiffOptions::default(),
            )
            .expect("diff_stat");
        let extra: &[&str] = if include_untracked {
            &["--include-untracked"]
        } else {
            &[]
        };
        assert_eq!(
            numstat_rows(&stat),
            git_numstat(dir.path(), extra),
            "include_untracked={include_untracked}"
        );
        let totals = stat.totals();
        assert_eq!(totals.files_changed, stat.files().len());
        assert_eq!(
            totals.insertions,
            stat.files()
                .iter()
                .map(|f| f.lines.insertions)
                .sum::<usize>()
        );
    }
}

#[test]
fn stash_diff_stat_omits_whitespace_only_files_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_fixture(dir.path());
    let repo = open_grit(dir.path());

    let stat = repo
        .stash()
        .diff_stat(
            0,
            &StashDiffOptions::default(),
            DiffOptions::default().with_whitespace(WhitespaceMode::ignore_space_change()),
        )
        .expect("diff_stat");
    let rows = numstat_rows(&stat);
    assert_eq!(rows, git_numstat(dir.path(), &["-b"]));
    assert!(
        !rows.iter().any(|row| row.ends_with("\tws.txt")),
        "{rows:?}"
    );
    #[cfg(unix)]
    assert!(rows.contains(&"0\t0\tmode.sh".to_owned()), "{rows:?}");
}

#[test]
fn stash_diff_is_in_path_order_with_untracked_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_fixture(dir.path());
    let repo = open_grit(dir.path());

    let entries = repo
        .stash()
        .diff(
            0,
            &StashDiffOptions {
                include_untracked: true,
            },
        )
        .expect("diff");
    let paths: Vec<&str> = entries.iter().map(|e| e.path()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted);
    assert!(paths.contains(&"aa-untracked.txt"));
}

#[test]
fn stash_diff_stat_reports_missing_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_fixture(dir.path());
    let repo = open_grit(dir.path());

    let err = repo
        .stash()
        .diff_stat(5, &StashDiffOptions::default(), DiffOptions::default())
        .expect_err("no stash@{5}");
    assert!(
        matches!(err, grit_lib::error::Error::StashNotFound { .. }),
        "{err:?}"
    );
}
