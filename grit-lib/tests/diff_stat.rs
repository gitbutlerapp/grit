//! `grit_lib::diff::DiffStat`: per-file counts, totals and change bars from listed file diffs.

use grit_lib::diff::{
    DiffDetail, DiffOptions, DiffStat, DiffStatus, DiffTotals, FileStat, LineStat, ListedFileDiff,
    StatBar, TreeDiffOptions,
};
use grit_lib::objects::{serialize_tree, ObjectId, ObjectKind, TreeEntry};
use grit_lib::repo::{InitOptions, Repository};

fn tree_with(repo: &Repository, entries: &[(&str, &[u8])]) -> ObjectId {
    let entries: Vec<TreeEntry> = entries
        .iter()
        .map(|(name, data)| TreeEntry {
            mode: 0o100644,
            name: name.as_bytes().to_vec(),
            oid: repo.odb().write(ObjectKind::Blob, data).expect("blob"),
        })
        .collect();
    repo.odb()
        .write(ObjectKind::Tree, &serialize_tree(&entries))
        .expect("tree")
}

fn listed(
    repo: &Repository,
    old: &ObjectId,
    new: &ObjectId,
    detail: DiffDetail,
) -> Vec<ListedFileDiff> {
    let entries = repo
        .diff()
        .tree_to_tree(Some(old), Some(new), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("tree diff");
    repo.diff()
        .files_for_entries(entries, detail, &DiffOptions::default())
        .expect("file diffs")
        .files
}

#[test]
fn stat_and_patch_details_give_the_same_diffstat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let old = tree_with(
        &repo,
        &[
            ("bin.dat", &[0, 1, 2]),
            ("edit.txt", b"a\nb\nc\nd\n"),
            ("gone.txt", b"x\ny\n"),
        ],
    );
    let new = tree_with(
        &repo,
        &[
            ("bin.dat", &[0, 1, 2, 3]),
            ("edit.txt", b"a\nB\nc\nd\ne\n"),
            ("new.txt", b"1\n2\n3\n"),
        ],
    );

    let from_stat = DiffStat::from_files(&listed(&repo, &old, &new, DiffDetail::Stat));
    let from_patch = DiffStat::from_files(&listed(&repo, &old, &new, DiffDetail::Patch));
    assert_eq!(from_stat, from_patch);

    let lines = |insertions, deletions| LineStat {
        insertions,
        deletions,
    };
    let bin_row = from_stat
        .files()
        .iter()
        .find(|f| f.path == "bin.dat")
        .expect("bin.dat row");
    assert!(bin_row.binary);
    assert_eq!(bin_row.old_size, 3);
    assert_eq!(bin_row.new_size, 4);

    let rows: Vec<_> = from_stat
        .files()
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.binary, f.lines))
        .collect();
    assert_eq!(
        rows,
        [
            ("bin.dat", DiffStatus::Modified, true, LineStat::default()),
            ("edit.txt", DiffStatus::Modified, false, lines(2, 1)),
            ("gone.txt", DiffStatus::Deleted, false, lines(0, 2)),
            ("new.txt", DiffStatus::Added, false, lines(3, 0)),
        ]
    );
    assert_eq!(
        from_stat.totals(),
        DiffTotals {
            files_changed: 4,
            insertions: 5,
            deletions: 3,
        }
    );
    assert_eq!(from_stat.max_changes(), 3);
    assert_eq!(
        from_stat.files()[1].lines.bar(from_stat.max_changes(), 40),
        StatBar { plus: 2, minus: 1 }
    );

    // Name-only listings carry no counts.
    let names = DiffStat::from_files(&listed(&repo, &old, &new, DiffDetail::NameOnly));
    assert_eq!(names.totals().files_changed, 4);
    assert_eq!(names.totals().insertions + names.totals().deletions, 0);
}

#[test]
fn diffstat_from_rows_sums_totals_in_order() {
    let row = |path: &str, insertions, deletions| FileStat {
        path: path.to_owned(),
        old_path: None,
        status: DiffStatus::Modified,
        binary: false,
        old_size: 0,
        new_size: 0,
        similarity: None,
        lines: LineStat {
            insertions,
            deletions,
        },
    };
    let stat: DiffStat = [row("z", 100, 0), row("a", 1, 1)].into_iter().collect();
    assert_eq!(stat.files()[0].path, "z");
    assert_eq!(stat.totals().insertions, 101);
    assert_eq!(stat.max_changes(), 100);
    assert_eq!(
        stat.files()[0].lines.bar(stat.max_changes(), 40),
        StatBar { plus: 40, minus: 0 }
    );
    assert_eq!(
        stat.files()[1].lines.bar(stat.max_changes(), 40),
        StatBar { plus: 0, minus: 1 }
    );
    assert_eq!(stat.clone().into_files().len(), 2);
    assert_eq!(DiffStat::new(Vec::new()), DiffStat::default());
}
