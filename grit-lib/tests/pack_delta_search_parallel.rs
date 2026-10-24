//! Deterministic windowed delta search across worker counts (1/2/4/8 threads).

use std::path::Path;
use std::process::Command;

use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack_objects::{build_pack_with_parallel_threads, PackBuildOptions};
use grit_lib::objects::ObjectId;
use grit_test_support::git_index_pack_stdin;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "T"),
    ("GIT_AUTHOR_EMAIL", "t@example.com"),
    ("GIT_COMMITTER_NAME", "T"),
    ("GIT_COMMITTER_EMAIL", "t@example.com"),
];

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
}

fn open_odb(dir: &Path) -> Odb {
    let git_dir = dir.join(".git");
    Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir)
}

fn git_validate_pack(pack: &[u8], odb: &Odb) {
    let out = git_index_pack_stdin(pack, odb.objects_dir(), &["--strict"]);
    assert!(out.ok, "git index-pack: {}", out.output);
}

fn assert_identical_search_and_pack(
    odb: &Odb,
    tip: ObjectId,
    opts: &PackBuildOptions,
    label: &str,
) {
    let baseline = build_pack_with_parallel_threads(odb, &[tip], &[], opts, Some(1), Some(1))
        .unwrap_or_else(|e| panic!("{label} baseline: {e}"));
    git_validate_pack(&baseline, odb);

    for threads in [2_usize, 4, 8] {
        let pack = build_pack_with_parallel_threads(odb, &[tip], &[], opts, Some(threads), Some(1))
            .unwrap_or_else(|e| panic!("{label} search threads={threads}: {e}"));
        assert_eq!(
            pack, baseline,
            "{label}: pack bytes differ with search threads={threads}"
        );
    }
}

fn build_delta_fixture() -> (tempfile::TempDir, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!("line {i:05} lorem ipsum dolor sit amet\n"));
    }
    for rev in 0..8 {
        body.push_str(&format!("--- edit {rev} ---\n"));
        std::fs::write(dir.join("big.txt"), body.as_bytes()).unwrap();
        git(dir, &["add", "big.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
    }
    let tip = rev_parse(dir, "HEAD");
    (tmp, tip)
}

#[test]
fn delta_search_threads_identical_bases_and_pack_bytes() {
    let (dir, tip) = build_delta_fixture();
    let odb = open_odb(dir.path());
    let opts = PackBuildOptions {
        delta: true,
        ..PackBuildOptions::default()
    };
    assert_identical_search_and_pack(&odb, tip, &opts, "delta fixture");
}

#[test]
fn delta_search_near_git_pack_objects_size_on_fixture() {
    let (dir, tip) = build_delta_fixture();
    let repo = dir.path();
    let odb = open_odb(repo);
    let opts = PackBuildOptions {
        delta: true,
        window: 10,
        max_depth: 50,
        ..PackBuildOptions::default()
    };
    let ours = build_pack_with_parallel_threads(&odb, &[tip], &[], &opts, Some(4), Some(1))
        .expect("grit pack");
    git_validate_pack(&ours, &odb);

    let git_size = {
        use std::io::Write as _;
        let mut cmd = Command::new("git");
        cmd.current_dir(repo)
            .args([
                "pack-objects",
                "--stdout",
                "--revs",
                "--delta-base-offset",
                "--window=10",
                "--depth=50",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (k, v) in GIT_ENV {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("git pack-objects");
        writeln!(child.stdin.as_mut().expect("stdin"), "{}", tip.to_hex()).unwrap();
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success(), "git pack-objects failed");
        out.stdout.len()
    };
    let ratio = ours.len() as f64 / git_size as f64;
    eprintln!(
        "delta_search size ratio ours={} git={} ratio={:.3}",
        ours.len(),
        git_size,
        ratio
    );
    assert!(
        ours.len() <= git_size * 11 / 10,
        "grit pack ({}) > 1.10× git pack-objects ({})",
        ours.len(),
        git_size
    );
}
