//! [`grit_lib::divergence`]: each scenario two heads of a shared branch can
//! be in, built commit by commit (with change ids, as `grit commit` writes
//! them) and classified from both sides.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::divergence::{classify, common_base, Did, Scenario};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    clock: std::cell::Cell<u64>,
}

fn git_in(dir: &Path, args: &[&str], input: Option<&str>, index: Option<&Path>) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd.spawn().unwrap();
    if let Some(input) = input {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A change id: 32 letters from `k` to `z`.
fn id(c: char) -> String {
    std::iter::repeat_n(c, 32).collect()
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        git_in(&dir, &["init", "-q", "-b", "main", "."], None, None);
        Self {
            _tmp: tmp,
            dir,
            clock: std::cell::Cell::new(1_700_000_000),
        }
    }

    /// A commit on `parent` setting `files` (`None` deletes), with change
    /// id `change` (or none).
    fn commit(
        &self,
        parent: Option<&str>,
        files: &[(&str, Option<&str>)],
        message: &str,
        change: Option<&str>,
    ) -> String {
        self.commit_parents(
            &parent.into_iter().collect::<Vec<_>>(),
            files,
            message,
            change,
        )
    }

    fn commit_parents(
        &self,
        parents: &[&str],
        files: &[(&str, Option<&str>)],
        message: &str,
        change: Option<&str>,
    ) -> String {
        let index = self.dir.join(".git/fixture-index");
        let _ = std::fs::remove_file(&index);
        match parents.first() {
            Some(p) => git_in(&self.dir, &["read-tree", p], None, Some(&index)),
            None => git_in(&self.dir, &["read-tree", "--empty"], None, Some(&index)),
        };
        for (path, content) in files {
            match content {
                Some(text) => {
                    let blob = git_in(
                        &self.dir,
                        &["hash-object", "-w", "--stdin"],
                        Some(text),
                        None,
                    );
                    git_in(
                        &self.dir,
                        &[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &format!("100644,{blob},{path}"),
                        ],
                        None,
                        Some(&index),
                    );
                }
                None => {
                    git_in(
                        &self.dir,
                        &["update-index", "--force-remove", path],
                        None,
                        Some(&index),
                    );
                }
            }
        }
        let tree = git_in(&self.dir, &["write-tree"], None, Some(&index));
        let at = self.clock.get();
        self.clock.set(at + 60);
        let mut text = format!("tree {tree}\n");
        for p in parents {
            text.push_str(&format!("parent {p}\n"));
        }
        text.push_str(&format!("author T <t@example.com> {at} +0000\n"));
        text.push_str(&format!("committer T <t@example.com> {at} +0000\n"));
        if let Some(change) = change {
            text.push_str(&format!("change-id {change}\n"));
        }
        text.push_str(&format!("\n{message}\n"));
        git_in(
            &self.dir,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            Some(&text),
            None,
        )
    }

    fn repo(&self) -> Repository {
        Repository::discover(Some(&self.dir), &RepositoryOptions::empty()).unwrap()
    }
}

fn oid(hex: &str) -> ObjectId {
    hex.parse().unwrap()
}

/// Classify `ours` against `theirs` over `base`, from both sides, and check
/// the main scenario each way.
fn check(
    f: &Fixture,
    main: &str,
    base: &str,
    ours: &str,
    theirs: &str,
    want: Scenario,
) -> grit_lib::divergence::Divergence {
    let repo = f.repo();
    let d = classify(&repo, &[oid(main)], Some(oid(base)), oid(ours), oid(theirs)).unwrap();
    assert_eq!(d.scenario, want, "ours vs theirs: {d:#?}");
    let back = classify(&repo, &[oid(main)], Some(oid(base)), oid(theirs), oid(ours)).unwrap();
    let mirrored = match want {
        Scenario::Ahead => Scenario::Behind,
        Scenario::Behind => Scenario::Ahead,
        other => other,
    };
    assert_eq!(back.scenario, mirrored, "theirs vs ours: {back:#?}");
    assert_eq!(want.needs_decision(), d.scenario.needs_decision());
    d
}

/// main, and a base of two commits on it: A (a.txt) and C (c.txt, d.txt).
struct Base {
    f: Fixture,
    main: String,
    a: String,
    b: String,
}

fn base() -> Base {
    let f = Fixture::new();
    let main = f.commit(
        None,
        &[
            ("README", Some("hello\n")),
            ("x.txt", Some("1\n2\n3\n4\n5\n6\n7\n8\n9\n")),
        ],
        "Start",
        Some(&id('k')),
    );
    let a = f.commit(
        Some(&main),
        &[("a.txt", Some("a1\na2\na3\n"))],
        "Add A",
        Some(&id('l')),
    );
    let b = f.commit(
        Some(&a),
        &[
            ("c.txt", Some("c1\nc2\nc3\n")),
            ("d.txt", Some("d1\nd2\nd3\n")),
        ],
        "Add C",
        Some(&id('m')),
    );
    Base { f, main, a, b }
}

#[test]
fn not_diverged() {
    let Base { f, main, a, b } = base();
    check(&f, &main, &b, &b, &b, Scenario::Same);
    let d = f.commit(Some(&b), &[("e.txt", Some("e\n"))], "Add D", Some(&id('n')));
    check(&f, &main, &b, &d, &b, Scenario::Ahead);
    check(&f, &main, &a, &a, &b, Scenario::Behind);
}

#[test]
fn added_both_and_conflicting() {
    let Base { f, main, b, .. } = base();
    let ours = f.commit(Some(&b), &[("e.txt", Some("e\n"))], "Add E", Some(&id('n')));
    let theirs = f.commit(Some(&b), &[("f.txt", Some("f\n"))], "Add F", Some(&id('o')));
    let d = check(&f, &main, &b, &ours, &theirs, Scenario::AddedBoth);
    assert!(d
        .changes
        .iter()
        .any(|c| c.ours == Did::New && c.theirs == Did::Absent));
    let ours = f.commit(
        Some(&b),
        &[("c.txt", Some("c1\nOURS\nc3\n"))],
        "Edit c",
        Some(&id('p')),
    );
    let theirs = f.commit(
        Some(&b),
        &[("c.txt", Some("c1\nTHEIRS\nc3\n"))],
        "Edit c too",
        Some(&id('q')),
    );
    let d = check(&f, &main, &b, &ours, &theirs, Scenario::AddedBothConflict);
    assert_eq!(d.conflicts, ["c.txt"]);
    assert!(
        d.summary("alice").contains("conflict in c.txt"),
        "{}",
        d.summary("alice")
    );
}

#[test]
fn amended_on_one_side() {
    let Base { f, main, a, b } = base();
    // They amend C; we add on top of the base.
    let theirs = f.commit(
        Some(&a),
        &[
            ("c.txt", Some("c1\nC2\nc3\n")),
            ("d.txt", Some("d1\nd2\nd3\n")),
        ],
        "Add C",
        Some(&id('m')),
    );
    let ours = f.commit(Some(&b), &[("e.txt", Some("e\n"))], "Add E", Some(&id('n')));
    let d = check(&f, &main, &b, &ours, &theirs, Scenario::RewroteOne);
    let c = d.changes.iter().find(|c| c.key == id('m')).unwrap();
    assert_eq!(
        (c.ours.clone(), c.theirs.clone()),
        (Did::Unchanged, Did::Amended)
    );
}

#[test]
fn amended_on_both_sides() {
    let Base { f, main, a, b } = base();
    let amend = |c: &str, d: &str, msg: &str| {
        f.commit(
            Some(&a),
            &[("c.txt", Some(c)), ("d.txt", Some(d))],
            msg,
            Some(&id('m')),
        )
    };
    // Different files: they combine.
    let ours = amend("c1\nOURS\nc3\n", "d1\nd2\nd3\n", "Add C");
    let theirs = amend("c1\nc2\nc3\n", "d1\nTHEIRS\nd3\n", "Add C");
    check(
        &f,
        &main,
        &b,
        &ours,
        &theirs,
        Scenario::AmendedBothCompatible,
    );
    // The same line: they conflict.
    let theirs = amend("c1\nTHEIRS\nc3\n", "d1\nd2\nd3\n", "Add C");
    let d = check(
        &f,
        &main,
        &b,
        &ours,
        &theirs,
        Scenario::AmendedBothConflicting,
    );
    assert_eq!(d.conflicts, ["c.txt"]);
    // A message on one side, content on the other.
    let reworded = amend("c1\nc2\nc3\n", "d1\nd2\nd3\n", "Add C, explained");
    check(
        &f,
        &main,
        &b,
        &ours,
        &reworded,
        Scenario::AmendedBothCompatible,
    );
    // The very same amend.
    let same = amend("c1\nOURS\nc3\n", "d1\nd2\nd3\n", "Add C");
    let d = classify(
        &f.repo(),
        &[oid(&main)],
        Some(oid(&b)),
        oid(&ours),
        oid(&same),
    )
    .unwrap();
    assert!(
        matches!(
            d.scenario,
            Scenario::Same | Scenario::SameContent | Scenario::SameRewrite
        ),
        "{d:#?}"
    );
}

#[test]
fn amended_different_commits() {
    let Base { f, main, b, .. } = base();
    let a2 = f.commit(
        Some(&main),
        &[("a.txt", Some("a1\nA2\na3\n"))],
        "Add A",
        Some(&id('l')),
    );
    let ours = f.commit(
        Some(&a2),
        &[
            ("c.txt", Some("c1\nc2\nc3\n")),
            ("d.txt", Some("d1\nd2\nd3\n")),
        ],
        "Add C",
        Some(&id('m')),
    );
    let a = f.commit(
        Some(&main),
        &[("a.txt", Some("a1\na2\na3\n"))],
        "Add A",
        Some(&id('l')),
    );
    let theirs = f.commit(
        Some(&a),
        &[
            ("c.txt", Some("c1\nC2\nc3\n")),
            ("d.txt", Some("d1\nd2\nd3\n")),
        ],
        "Add C",
        Some(&id('m')),
    );
    check(&f, &main, &b, &ours, &theirs, Scenario::AmendedDifferent);
}

#[test]
fn dropped() {
    let Base { f, main, a, b } = base();
    // They drop C; we add on top.
    let ours = f.commit(Some(&b), &[("e.txt", Some("e\n"))], "Add E", Some(&id('n')));
    let theirs = f.commit(Some(&a), &[("f.txt", Some("f\n"))], "Add F", Some(&id('o')));
    let d = check(&f, &main, &b, &ours, &theirs, Scenario::DroppedOne);
    assert!(d.summary("alice").contains("\"Add C\""));
    // Both drop it.
    let ours = f.commit(Some(&a), &[("e.txt", Some("e\n"))], "Add E", Some(&id('n')));
    check(&f, &main, &b, &ours, &theirs, Scenario::DroppedBoth);
    // We drop it; they amend it.
    let theirs = f.commit(
        Some(&a),
        &[
            ("c.txt", Some("c1\nC2\nc3\n")),
            ("d.txt", Some("d1\nd2\nd3\n")),
        ],
        "Add C",
        Some(&id('m')),
    );
    let d = check(
        &f,
        &main,
        &b,
        &ours,
        &theirs,
        Scenario::DroppedOneAmendedOther,
    );
    assert!(d
        .summary("alice")
        .starts_with("You dropped \"Add C\", and alice amended it"));
}

/// main, and a base of three commits: A, C and F.
fn three() -> (Fixture, String, String) {
    let f = Fixture::new();
    let main = f.commit(
        None,
        &[("README", Some("hello\n"))],
        "Start",
        Some(&id('k')),
    );
    let a = f.commit(
        Some(&main),
        &[("a.txt", Some("a\n"))],
        "Add A",
        Some(&id('l')),
    );
    let c = f.commit(Some(&a), &[("c.txt", Some("c\n"))], "Add C", Some(&id('m')));
    let b = f.commit(Some(&c), &[("f.txt", Some("f\n"))], "Add F", Some(&id('o')));
    (f, main, b)
}

/// The commits A, C and F (by letter) on `main`, in `order`, `amend`ed
/// where named.
fn order(f: &Fixture, main: &str, order: &[char], amend: Option<char>) -> String {
    let mut at = main.to_owned();
    for letter in order {
        let (file, key) = match letter {
            'A' => ("a.txt", 'l'),
            'C' => ("c.txt", 'm'),
            _ => ("f.txt", 'o'),
        };
        let text = if amend == Some(*letter) {
            format!("{} amended\n", letter.to_ascii_lowercase())
        } else {
            format!("{}\n", letter.to_ascii_lowercase())
        };
        at = f.commit(
            Some(&at),
            &[(file, Some(&text))],
            &format!("Add {letter}"),
            Some(&id(key)),
        );
    }
    at
}

#[test]
fn reordered() {
    let (f, main, b) = three();
    let ours = order(&f, &main, &['C', 'A', 'F'], None);
    let theirs = order(&f, &main, &['A', 'F', 'C'], None);
    check(&f, &main, &b, &ours, &theirs, Scenario::ReorderedBoth);
    // We reorder; they amend.
    let theirs = order(&f, &main, &['A', 'C', 'F'], Some('A'));
    let d = check(
        &f,
        &main,
        &b,
        &ours,
        &theirs,
        Scenario::AmendedOneReorderedOther,
    );
    let a = d.changes.iter().find(|c| c.key == id('l')).unwrap();
    assert_eq!(a.theirs, Did::Amended);
}

#[test]
fn split() {
    let Base { f, main, a, b } = base();
    let ours = f.commit(Some(&b), &[("e.txt", Some("e\n"))], "Add E", Some(&id('n')));
    // They split C into c.txt (keeping its id) and d.txt.
    let c1 = f.commit(
        Some(&a),
        &[("c.txt", Some("c1\nc2\nc3\n"))],
        "Add C",
        Some(&id('m')),
    );
    let theirs = f.commit(
        Some(&c1),
        &[("d.txt", Some("d1\nd2\nd3\n"))],
        "Add D",
        Some(&id('p')),
    );
    let d = check(&f, &main, &b, &ours, &theirs, Scenario::SplitOne);
    let c = d.changes.iter().find(|c| c.key == id('m')).unwrap();
    assert!(
        matches!(&c.theirs, Did::Split(pieces) if pieces.contains(&id('p'))),
        "{c:?}"
    );
    let piece = d.changes.iter().find(|c| c.key == id('p')).unwrap();
    assert_eq!(piece.theirs, Did::SplitFrom(id('m')));
    // We split it too, the other way round.
    let d1 = f.commit(
        Some(&a),
        &[("d.txt", Some("d1\nd2\nd3\n"))],
        "Add C",
        Some(&id('m')),
    );
    let ours = f.commit(
        Some(&d1),
        &[("c.txt", Some("c1\nc2\nc3\n"))],
        "Add C2",
        Some(&id('q')),
    );
    check(&f, &main, &b, &ours, &theirs, Scenario::SplitBoth);
    // We amend d.txt in C: it fits their d.txt piece.
    let ours = f.commit(
        Some(&a),
        &[
            ("c.txt", Some("c1\nc2\nc3\n")),
            ("d.txt", Some("d1\nD2\nd3\n")),
        ],
        "Add C",
        Some(&id('m')),
    );
    check(
        &f,
        &main,
        &b,
        &ours,
        &theirs,
        Scenario::SplitOneAmendedOther,
    );
}

#[test]
fn squashed() {
    let Base { f, main, b, .. } = base();
    // We squash A into C; they amend A.
    let ours = f.commit(
        Some(&main),
        &[
            ("a.txt", Some("a1\na2\na3\n")),
            ("c.txt", Some("c1\nc2\nc3\n")),
            ("d.txt", Some("d1\nd2\nd3\n")),
        ],
        "Add A and C",
        Some(&id('m')),
    );
    let a2 = f.commit(
        Some(&main),
        &[("a.txt", Some("a1\nA2\na3\n"))],
        "Add A",
        Some(&id('l')),
    );
    let theirs = f.commit(
        Some(&a2),
        &[
            ("c.txt", Some("c1\nc2\nc3\n")),
            ("d.txt", Some("d1\nd2\nd3\n")),
        ],
        "Add C",
        Some(&id('m')),
    );
    let d = check(
        &f,
        &main,
        &b,
        &ours,
        &theirs,
        Scenario::SquashedOneAmendedOther,
    );
    let a = d.changes.iter().find(|c| c.key == id('l')).unwrap();
    assert_eq!(a.ours, Did::SquashedInto(id('m')));
}

#[test]
fn rebased_and_reset() {
    let Base { f, main, b, .. } = base();
    let main2 = f.commit(
        Some(&main),
        &[("README", Some("hello again\n"))],
        "Newer main",
        Some(&id('z')),
    );
    let rebase = |onto: &str| {
        let a = f.commit(
            Some(onto),
            &[("a.txt", Some("a1\na2\na3\n"))],
            "Add A",
            Some(&id('l')),
        );
        f.commit(
            Some(&a),
            &[
                ("c.txt", Some("c1\nc2\nc3\n")),
                ("d.txt", Some("d1\nd2\nd3\n")),
            ],
            "Add C",
            Some(&id('m')),
        )
    };
    // They rebase onto a newer main; we add on top of the base.
    let theirs = rebase(&main2);
    let ours = f.commit(Some(&b), &[("e.txt", Some("e\n"))], "Add E", Some(&id('n')));
    // The floor is main as it is now.
    let _ = main;
    check(&f, &main2, &b, &ours, &theirs, Scenario::RebasedOne);
    // Both rebase onto the same main, as different commits: the same.
    let ours = rebase(&main2);
    check(&f, &main2, &b, &ours, &theirs, Scenario::SameContent);
    // They keep only A, on a newer main.
    let theirs = f.commit(
        Some(&main2),
        &[("a.txt", Some("a1\na2\na3\n"))],
        "Add A",
        Some(&id('l')),
    );
    let three_c = f.commit(Some(&b), &[("g.txt", Some("g\n"))], "Add G", Some(&id('p')));
    let d = classify(
        &f.repo(),
        &[oid(&main2)],
        Some(oid(&three_c)),
        oid(&three_c),
        oid(&theirs),
    )
    .unwrap();
    assert!(d.scenarios.contains(&Scenario::ResetOne), "{d:#?}");
}

#[test]
fn merge_commits_and_missing_ids() {
    let Base { f, main, a, b } = base();
    let side = f.commit(
        Some(&main),
        &[("s.txt", Some("s\n"))],
        "Side",
        Some(&id('s')),
    );
    let ours = f.commit_parents(
        &[&b, &side],
        &[("s.txt", Some("s\n"))],
        "Merge",
        Some(&id('t')),
    );
    let theirs = f.commit(Some(&b), &[("f.txt", Some("f\n"))], "Add F", Some(&id('o')));
    check(&f, &main, &b, &ours, &theirs, Scenario::MergeCommit);

    // Without change ids, an amend reads as a drop and a new commit.
    let c = f.commit(Some(&a), &[("c.txt", Some("c1\nc2\nc3\n"))], "Add C", None);
    let ours = f.commit(Some(&c), &[("e.txt", Some("e\n"))], "Add E", None);
    let theirs = f.commit(Some(&a), &[("c.txt", Some("c1\nC2\nc3\n"))], "Add C", None);
    let d = check(&f, &main, &c, &ours, &theirs, Scenario::NoChangeIds);
    assert!(d.matched_without_ids);
}

#[test]
fn the_base_is_the_newest_head_both_had() {
    let Base { f, main, a, b } = base();
    let ours = f.commit(Some(&b), &[("e.txt", Some("e\n"))], "Add E", Some(&id('n')));
    let theirs = f.commit(Some(&a), &[("f.txt", Some("f\n"))], "Add F", Some(&id('o')));
    let repo = f.repo();
    // Both held b at some point.
    let base = common_base(
        &repo,
        oid(&ours),
        oid(&theirs),
        &[oid(&b), oid(&a)],
        &[oid(&b)],
    )
    .unwrap();
    assert_eq!(base, Some(oid(&b)));
    // Nothing in common: the merge base.
    let base = common_base(&repo, oid(&ours), oid(&theirs), &[], &[]).unwrap();
    assert_eq!(base, Some(oid(&a)));
    let _ = main;
}

#[test]
fn every_scenario_has_a_code_and_a_rank() {
    let all = Scenario::all();
    assert_eq!(all.len(), 28);
    let codes: std::collections::HashSet<&str> = all.iter().map(|s| s.code()).collect();
    assert_eq!(codes.len(), 28);
    assert_eq!(all[0], Scenario::MergeCommit);
    assert!(all.iter().take_while(|s| s.needs_decision()).count() == 10);
}
