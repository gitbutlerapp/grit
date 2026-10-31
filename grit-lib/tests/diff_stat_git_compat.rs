//! `grit_lib::diff::DiffStat` counts match system `git diff --numstat` / `--shortstat`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    build_file_diffs, DiffDetail, DiffOptions, DiffStat, DiffStatus, DiffTarget, RenameDetection,
};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::Repository;
use grit_test_support::{git, rev_parse, write_file};

fn diffstat(repo: &Repository, detail: DiffDetail, options: DiffOptions) -> DiffStat {
    let old = rev_parse(repo.work_tree().expect("work tree"), "HEAD~1^{tree}");
    let new = rev_parse(repo.work_tree().expect("work tree"), "HEAD^{tree}");
    let entries = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new))
        .expect("tree diff");
    let config = repo.config().expect("config");
    let out = build_file_diffs(
        repo.odb(),
        entries,
        detail,
        options,
        config.as_ref(),
        &ThreadPolicy::default(),
    )
    .expect("file diffs");
    DiffStat::from_files(&out.files)
}

#[test]
fn diffstat_matches_git_numstat_and_shortstat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
    write_file(root, "big.txt", &long);
    write_file(root, "edit.txt", "a\nb\nc\nd\ne\n");
    write_file(root, "gone.txt", "x\ny\nz\n");
    std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3]).expect("binary");
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "-m",
            "one",
        ],
    );

    let edited: String = long.replace("line 1", "LINE 1");
    write_file(root, "big.txt", &edited);
    write_file(root, "edit.txt", "a\nB\nc\ne\nf\ng\n");
    std::fs::remove_file(root.join("gone.txt")).expect("remove");
    write_file(root, "new.txt", "1\n2\n");
    std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3, 4]).expect("binary");
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "-m",
            "two",
        ],
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let numstat = git(
        root,
        &["diff", "--no-renames", "--numstat", "HEAD~1", "HEAD"],
    );
    let shortstat = git(
        root,
        &["diff", "--no-renames", "--shortstat", "HEAD~1", "HEAD"],
    );

    for detail in [DiffDetail::Stat, DiffDetail::Patch] {
        let stat = diffstat(
            &repo,
            detail,
            DiffOptions::default().with_renames(RenameDetection::Off),
        );
        let ours: String = stat
            .files()
            .iter()
            .map(|f| {
                if f.binary {
                    format!("-\t-\t{}\n", f.path)
                } else {
                    format!(
                        "{}\t{}\t{}\n",
                        f.lines.insertions, f.lines.deletions, f.path
                    )
                }
            })
            .collect();
        assert_eq!(ours, numstat, "{detail:?}");

        let totals = stat.totals();
        let expected = format!(
            " {} files changed, {} insertions(+), {} deletions(-)\n",
            totals.files_changed, totals.insertions, totals.deletions
        );
        assert_eq!(expected, shortstat, "{detail:?}");
    }
}

#[test]
fn diffstat_honors_rename_detection_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let body: String = (0..80).map(|i| format!("shared line {i}\n")).collect();
    write_file(root, "dir/old_name.txt", &(body.clone() + "tail old\n"));
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );

    write_file(root, "dir/new_name.txt", &(body + "tail new\n"));
    std::fs::remove_file(root.join("dir/old_name.txt")).expect("remove old path");
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "-m",
            "rename",
        ],
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let base = rev_parse(root, "HEAD~1");
    let tip = rev_parse(root, "HEAD");
    let base_tree = rev_parse(root, "HEAD~1^{tree}");
    let tip_tree = rev_parse(root, "HEAD^{tree}");

    let git_numstat = git(root, &["diff", "--numstat", "HEAD~1", "HEAD"]);
    let git_shortstat = git(root, &["diff", "--shortstat", "HEAD~1", "HEAD"]);
    let git_numstat_no_renames = git(
        root,
        &["diff", "--no-renames", "--numstat", "HEAD~1", "HEAD"],
    );

    let stat_with_renames = diffstat(&repo, DiffDetail::Stat, DiffOptions::default());
    assert_eq!(
        stat_with_renames
            .files()
            .iter()
            .filter(|f| f.path == "dir/new_name.txt")
            .count(),
        1,
        "rename should collapse to one path"
    );
    assert!(
        stat_with_renames
            .files()
            .iter()
            .all(|f| f.path != "dir/old_name.txt"),
        "old path should not appear as a separate deletion row"
    );

    let ours_numstat: String = stat_with_renames
        .files()
        .iter()
        .map(|f| {
            format!(
                "{}\t{}\t{}\n",
                f.lines.insertions, f.lines.deletions, f.path
            )
        })
        .collect();
    assert_eq!(ours_numstat, git_numstat);

    let totals = stat_with_renames.totals();
    let expected_short = format!(
        " {} files changed, {} insertions(+), {} deletions(-)\n",
        totals.files_changed, totals.insertions, totals.deletions
    );
    assert_eq!(expected_short, git_shortstat);

    let stat_off = diffstat(
        &repo,
        DiffDetail::Stat,
        DiffOptions::default().with_renames(RenameDetection::Off),
    );
    assert_eq!(stat_off.files().len(), 2, "delete + add without renames");
    let ours_no_renames: String = stat_off
        .files()
        .iter()
        .map(|f| {
            format!(
                "{}\t{}\t{}\n",
                f.lines.insertions, f.lines.deletions, f.path
            )
        })
        .collect();
    assert_eq!(ours_no_renames, git_numstat_no_renames);
    assert!(
        totals.insertions + totals.deletions
            <= stat_off.totals().insertions + stat_off.totals().deletions,
        "rename pairing should not inflate line totals vs delete+add"
    );

    let config = repo.config().expect("config");
    let via_files = repo
        .diff()
        .files(
            &DiffTarget::Trees {
                old: Some(base),
                new: tip,
            },
            &[],
            &DiffOptions::default(),
        )
        .expect("Diff::files");
    assert_eq!(via_files.len(), stat_with_renames.files().len());
    assert!(
        via_files.iter().any(|f| f.status == DiffStatus::Renamed),
        "Diff::files should report rename"
    );

    let via_tree_ids = repo
        .diff()
        .files(
            &DiffTarget::Trees {
                old: Some(base_tree),
                new: tip_tree,
            },
            &[],
            &DiffOptions::default(),
        )
        .expect("tree ids");
    assert_eq!(via_tree_ids.len(), via_files.len());

    let blob = repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"not a tree or commit")
        .expect("blob");
    let err = repo.diff().files(
        &DiffTarget::Trees {
            old: Some(blob),
            new: tip,
        },
        &[],
        &DiffOptions::default(),
    );
    assert!(err.is_err(), "blob id must not be accepted as a tree side");
    let _ = blob;
    let _ = tip;
    let _ = base;
}
