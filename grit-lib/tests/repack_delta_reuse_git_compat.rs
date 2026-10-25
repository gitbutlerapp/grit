//! Full repack keeps stored deltas, recomputes them with `force_recompress`, and
//! writes packs the system `git` reads, checked against `git repack -ad` / `-adf`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{repack_all, RepackOptions};
use grit_lib::repo::Repository;
use tempfile::TempDir;

const COMMITS: usize = 240;

fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_command(dir).args(args).output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Content of `path` at `version`: a few hundred stable lines with a handful
/// rewritten per version, so consecutive versions delta well against each other.
fn file_body(path: &str, version: usize) -> String {
    let mut body = String::new();
    for line in 0..120 {
        if line % 17 == version % 17 || line == version % 120 {
            body.push_str(&format!(
                "{path}: line {line} edited in version {version}\n"
            ));
        } else {
            body.push_str(&format!(
                "{path}: stable line {line} with some padding text\n"
            ));
        }
    }
    body
}

/// A repository whose history edits two files per commit across a nested tree,
/// imported with `git fast-import` (fast, and fully deterministic).
fn history_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    let paths: Vec<String> = (0..6)
        .flat_map(|d| {
            (0..3).flat_map(move |s| (0..3).map(move |f| format!("dir{d}/sub{s}/file{f}.txt")))
        })
        .collect();
    let mut versions: HashMap<&str, usize> = HashMap::new();
    let mut stream = Vec::new();
    let mut notes: Vec<String> = (0..200).map(|i| format!("original line {i}")).collect();
    for c in 0..COMMITS {
        let msg = format!("commit {c}\n");
        writeln!(stream, "commit refs/heads/main").expect("write");
        writeln!(stream, "mark :{}", c + 1).expect("write");
        writeln!(
            stream,
            "committer T <t@example.com> {} +0000",
            1_700_000_000 + c * 60
        )
        .expect("write");
        write!(stream, "data {}\n{msg}", msg.len()).expect("write");
        if c > 0 {
            writeln!(stream, "from :{c}").expect("write");
        }
        let touched: Vec<&String> = if c == 0 {
            paths.iter().collect()
        } else {
            vec![
                &paths[(c * 7) % paths.len()],
                &paths[(c * 13 + 5) % paths.len()],
            ]
        };
        for path in touched {
            let v = versions.entry(path.as_str()).or_insert(0);
            *v += 1;
            let body = file_body(path, *v);
            write!(
                stream,
                "M 100644 inline {path}\ndata {}\n{body}\n",
                body.len()
            )
            .expect("write");
        }
        // One file changes in every commit, each version rewriting a line and adding
        // one: the closest version is always the best base, so its history forms
        // long delta chains.
        let at = (c * 37) % notes.len();
        notes[at] = format!("line rewritten in commit {c}");
        notes.push(format!("line added in commit {c}"));
        let body = notes.join("\n");
        write!(
            stream,
            "M 100644 inline NOTES\ndata {}\n{body}\n",
            body.len()
        )
        .expect("write");
    }
    let mut child = git_command(dir)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&stream)
        .expect("feed fast-import");
    assert!(child.wait().expect("fast-import").success());
    git(dir, &["reset", "-q", "--hard", "main"]);
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("mkdir");
    for entry in std::fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("copy");
        }
    }
}

fn packs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir.join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect();
    out.sort();
    out
}

fn only_pack(dir: &Path) -> PathBuf {
    let all = packs(dir);
    assert_eq!(
        all.len(),
        1,
        "expected one pack in {}: {all:?}",
        dir.display()
    );
    all.into_iter().next().expect("pack")
}

/// Per-object rows of `git verify-pack -v`: kind, delta base (if any), depth.
struct PackStats {
    bytes: u64,
    rows: HashMap<String, (String, Option<String>, usize)>,
}

impl PackStats {
    fn of(dir: &Path) -> Self {
        let pack = only_pack(dir);
        let out = git(
            dir,
            &["verify-pack", "-v", pack.to_str().expect("utf8 path")],
        );
        let mut rows = HashMap::new();
        for line in out.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 5 || fields[0].len() != 40 {
                continue;
            }
            let (base, depth) = if fields.len() >= 7 {
                (
                    Some(fields[6].to_owned()),
                    fields[5].parse().expect("depth"),
                )
            } else {
                (None, 0)
            };
            rows.insert(fields[0].to_owned(), (fields[1].to_owned(), base, depth));
        }
        Self {
            bytes: std::fs::metadata(&pack).expect("pack size").len(),
            rows,
        }
    }

    fn deltas(&self, kind: &str) -> usize {
        self.rows
            .values()
            .filter(|(k, base, _)| k == kind && base.is_some())
            .count()
    }

    fn total_deltas(&self) -> usize {
        self.rows
            .values()
            .filter(|(_, base, _)| base.is_some())
            .count()
    }

    fn max_depth(&self) -> usize {
        self.rows.values().map(|(_, _, d)| *d).max().unwrap_or(0)
    }
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo")
}

fn grit_repack(dir: &Path, force: bool) {
    let mut opts = RepackOptions::default();
    opts.force_recompress = force;
    let stats = repack_all(&open(dir), &opts).expect("repack");
    assert!(stats.objects_packed.is_some_and(|n| n > 0), "{stats:?}");
}

fn assert_git_reads(dir: &Path, objects_before: &str) {
    git(dir, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git(dir, &["rev-list", "--objects", "--all"]),
        objects_before
    );
}

#[test]
fn default_repack_copies_every_stored_delta() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("repo");
    std::fs::create_dir(&dir).expect("mkdir");
    history_repo(&dir);
    git(&dir, &["repack", "-adfq"]);
    let before = PackStats::of(&dir);
    let objects = git(&dir, &["rev-list", "--objects", "--all"]);
    assert!(
        before.total_deltas() > COMMITS,
        "fixture should be delta-heavy"
    );

    grit_repack(&dir, false);

    let after = PackStats::of(&dir);
    assert_git_reads(&dir, &objects);
    for (oid, (kind, base, _)) in &before.rows {
        let (new_kind, new_base, _) = &after.rows[oid];
        assert_eq!(kind, new_kind, "{oid}");
        assert_eq!(base, new_base, "{oid} must keep its stored delta base");
    }
    // Only delta headers change (OFS distances in the new layout).
    assert!(
        after.bytes.abs_diff(before.bytes) * 100 <= before.bytes,
        "pack size changed from {} to {}",
        before.bytes,
        after.bytes
    );
}

#[test]
fn forced_repack_stays_within_git_pack_size() {
    let tmp = TempDir::new().expect("tempdir");
    let grit_dir = tmp.path().join("grit");
    let git_dir = tmp.path().join("git");
    std::fs::create_dir(&grit_dir).expect("mkdir");
    history_repo(&grit_dir);
    // A pack without any deltas, like one written by a packer that did not search.
    git(&grit_dir, &["repack", "-adfq", "--window=0"]);
    assert_eq!(PackStats::of(&grit_dir).total_deltas(), 0);
    copy_dir(&grit_dir, &git_dir);
    let objects = git(&grit_dir, &["rev-list", "--objects", "--all"]);

    grit_repack(&grit_dir, true);
    git(&git_dir, &["repack", "-adfq"]);

    let ours = PackStats::of(&grit_dir);
    let theirs = PackStats::of(&git_dir);
    assert_git_reads(&grit_dir, &objects);
    assert!(
        ours.bytes * 10 <= theirs.bytes * 12,
        "grit pack {} bytes vs git {} bytes",
        ours.bytes,
        theirs.bytes
    );
    assert!(ours.deltas("blob") > 0 && ours.deltas("tree") > 0);
    assert!(ours.max_depth() <= 50, "depth {}", ours.max_depth());
}

#[test]
fn repack_cuts_stored_chains_deeper_than_pack_depth() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("repo");
    std::fs::create_dir(&dir).expect("mkdir");
    history_repo(&dir);
    git(&dir, &["repack", "-adfq", "--depth=250"]);
    let before = PackStats::of(&dir);
    assert!(
        before.max_depth() > 50,
        "fixture depth {}",
        before.max_depth()
    );
    let objects = git(&dir, &["rev-list", "--objects", "--all"]);

    grit_repack(&dir, false);

    let after = PackStats::of(&dir);
    assert_git_reads(&dir, &objects);
    assert!(after.max_depth() <= 50, "depth {}", after.max_depth());
    // Cut links are searched again, so almost every delta survives.
    assert!(
        after.total_deltas() * 100 >= before.total_deltas() * 95,
        "{} deltas before, {} after",
        before.total_deltas(),
        after.total_deltas()
    );
}

#[test]
fn repack_deltifies_loose_objects_against_packed_history() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("repo");
    std::fs::create_dir(&dir).expect("mkdir");
    history_repo(&dir);
    git(&dir, &["repack", "-adq"]);
    let path = "dir2/sub1/file0.txt";
    std::fs::write(dir.join(path), file_body(path, 999)).expect("write");
    git(&dir, &["commit", "-qam", "loose edit"]);
    let new_blob = git(&dir, &["rev-parse", &format!("HEAD:{path}")])
        .trim()
        .to_owned();
    let objects = git(&dir, &["rev-list", "--objects", "--all"]);

    grit_repack(&dir, false);

    let after = PackStats::of(&dir);
    assert_git_reads(&dir, &objects);
    let (kind, base, _) = &after.rows[&new_blob];
    assert_eq!(kind, "blob");
    assert!(
        base.is_some(),
        "loose blob {new_blob} should become a delta"
    );
    assert!(
        std::fs::read_dir(dir.join(".git/objects"))
            .expect("objects dir")
            .filter_map(|e| e.ok())
            .all(|e| e.file_name().len() != 2),
        "loose objects should be pruned after repack"
    );
}

#[test]
fn forced_repack_of_the_same_repository_is_deterministic() {
    let tmp = TempDir::new().expect("tempdir");
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    std::fs::create_dir(&first).expect("mkdir");
    history_repo(&first);
    copy_dir(&first, &second);

    grit_repack(&first, true);
    grit_repack(&second, true);

    let name = |dir: &Path| {
        only_pack(dir)
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned()
    };
    assert_eq!(name(&first), name(&second));
}

#[test]
fn git_repacks_a_grit_forced_pack() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("repo");
    std::fs::create_dir(&dir).expect("mkdir");
    history_repo(&dir);
    let objects = git(&dir, &["rev-list", "--objects", "--all"]);

    grit_repack(&dir, true);
    git(&dir, &["repack", "-adq"]);
    assert_git_reads(&dir, &objects);

    // Packs changed behind this process's back.
    open(&dir).odb().invalidate_packs();
    grit_repack(&dir, false);
    assert_git_reads(&dir, &objects);
}
