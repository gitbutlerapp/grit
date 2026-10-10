//! Round-trip and compatibility tests for [`grit_lib::bundle`] vs system `git bundle`.

use std::path::Path;
use std::process::Command;

use grit_lib::bundle::{read_header, write_bundle, Bundle, BundleError, BundleSpec};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: stderr={} stdout={}",
        args,
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn git_ok(dir: &Path, args: &[&str]) {
    git(dir, args);
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
}

fn fsck_strict(dir: &Path) {
    git_ok(dir, &["fsck", "--strict"]);
}

fn init_repo_with_two_commits(dir: &Path) -> (ObjectId, ObjectId) {
    git_ok(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git_ok(dir, &["add", "a.txt"]);
    git_ok(dir, &["commit", "-q", "-m", "first"]);
    let c1 = rev_parse(dir, "HEAD");
    std::fs::write(dir.join("b.txt"), b"two\n").unwrap();
    git_ok(dir, &["add", "b.txt"]);
    git_ok(dir, &["commit", "-q", "-m", "second"]);
    let c2 = rev_parse(dir, "HEAD");
    (c1, c2)
}

fn grit_repo(dir: &Path) -> Repository {
    Repository::discover(Some(dir)).expect("open grit repo")
}

#[test]
fn grit_bundle_full_and_incremental_git_verify_clone_fsck() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path();
    let (c1, c2) = init_repo_with_two_commits(src);
    let repo = grit_repo(src);

    let full_path = src.join("full.bundle");
    {
        let mut f = std::fs::File::create(&full_path).expect("create");
        write_bundle(
            &repo,
            &BundleSpec {
                include: vec![(c2, "refs/heads/main".to_owned())],
                ..Default::default()
            },
            &mut f,
        )
        .expect("write full bundle");
    }
    git_ok(src, &["bundle", "verify", full_path.to_str().unwrap()]);
    let heads = git(src, &["bundle", "list-heads", full_path.to_str().unwrap()]);
    assert!(heads.contains("refs/heads/main"));

    let clone_full = src.join("clone-full");
    git_ok(
        src,
        &[
            "clone",
            "-q",
            full_path.to_str().unwrap(),
            clone_full.to_str().unwrap(),
        ],
    );
    fsck_strict(&clone_full);

    let inc_path = src.join("inc.bundle");
    {
        let mut f = std::fs::File::create(&inc_path).expect("create");
        write_bundle(
            &repo,
            &BundleSpec {
                include: vec![(c2, "refs/heads/main".to_owned())],
                exclude: vec![c1],
                ..Default::default()
            },
            &mut f,
        )
        .expect("write incremental bundle");
    }
    git_ok(src, &["bundle", "verify", inc_path.to_str().unwrap()]);
    let heads_inc = git(src, &["bundle", "list-heads", inc_path.to_str().unwrap()]);
    assert!(heads_inc.contains("refs/heads/main"));
    // Incremental bundles require prerequisites locally; fetch after seeding c1.
    let inc_fetch = src.join("inc-fetch");
    std::fs::create_dir_all(&inc_fetch).expect("inc-fetch dir");
    git_ok(&inc_fetch, &["init", "-q", "--bare", "-b", "main", "."]);
    git_ok(
        &inc_fetch,
        &[
            "fetch",
            src.to_str().unwrap(),
            &format!("{}:refs/heads/base", c1.to_hex()),
        ],
    );
    git_ok(
        &inc_fetch,
        &[
            "fetch",
            inc_path.to_str().unwrap(),
            "refs/heads/main:refs/heads/main",
        ],
    );
    fsck_strict(&inc_fetch);
}

#[test]
fn git_created_bundles_parse_verify_unbundle_fsck() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path();
    let (_c1, c2) = init_repo_with_two_commits(src);
    git_ok(src, &["tag", "-a", "v1", "-m", "tag"]);
    let full = src.join("git-full.bundle");
    git_ok(
        src,
        &[
            "bundle",
            "create",
            full.to_str().unwrap(),
            "--all",
            "--tags",
        ],
    );

    let consumer_git = tempfile::tempdir().expect("consumer");
    let consumer = consumer_git.path();
    git_ok(consumer, &["init", "-q", "--bare", "-b", "main", "."]);
    let bare = grit_repo(consumer);

    let bundle = Bundle::open(&full).expect("open git bundle");
    let report = bundle.verify(&bare).expect("verify");
    assert!(report.is_ok(), "full bundle should need no prerequisites");
    let refs = bundle.unbundle(&bare).expect("unbundle");
    for r in &refs {
        git_ok(consumer, &["update-ref", &r.name, &r.oid.to_hex()]);
    }
    fsck_strict(consumer);

    let inc = src.join("git-inc.bundle");
    let c1 = rev_parse(src, "HEAD~1");
    git_ok(
        src,
        &[
            "bundle",
            "create",
            inc.to_str().unwrap(),
            &format!("{}..{}", c1.to_hex(), c2.to_hex()),
            "main",
        ],
    );

    let empty_git = tempfile::tempdir().expect("empty");
    git_ok(
        empty_git.path(),
        &["init", "-q", "--bare", "-b", "main", "."],
    );
    let empty = grit_repo(empty_git.path());
    let bundle = Bundle::open(&inc).expect("open inc");
    let report = bundle.verify(&empty).expect("verify");
    assert!(
        !report.missing_prerequisites.is_empty(),
        "incremental bundle should report missing prerequisite"
    );
    let err = report.into_result().unwrap_err();
    assert!(matches!(
        err,
        BundleError::MissingPrerequisite { oid, .. } if oid == c1
    ));

    // Consumer already has the full object graph from the first unbundle; c1 is
    // reachable via refs/heads/main, so incremental verify/unbundle should succeed.
    let bundle = Bundle::open(&inc).expect("reopen");
    assert!(bundle.verify(&bare).expect("verify").is_ok());
    let refs = bundle.unbundle(&bare).expect("unbundle");
    assert!(refs.iter().any(|r| r.name == "refs/heads/main"));
    fsck_strict(consumer);
}

#[test]
fn git_v3_sha256_bundle_round_trips_when_supported() {
    let probe_dir = tempfile::tempdir().expect("probe dir");
    let probe = Command::new("git")
        .args(["init", "--object-format=sha256", "--bare", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(probe_dir.path())
        .output()
        .expect("probe");
    if !probe.status.success() {
        eprintln!("SKIP: git lacks --object-format=sha256");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path();
    git_ok(
        src,
        &["init", "-q", "--object-format=sha256", "-b", "main", "."],
    );
    std::fs::write(src.join("f"), b"x\n").unwrap();
    git_ok(src, &["add", "f"]);
    git_ok(src, &["commit", "-q", "-m", "sha256"]);
    let bundle_path = src.join("sha256.bundle");
    git_ok(
        src,
        &["bundle", "create", bundle_path.to_str().unwrap(), "HEAD"],
    );

    let consumer = tempfile::tempdir().expect("consumer");
    git_ok(
        consumer.path(),
        &["init", "-q", "--bare", "--object-format=sha256", "."],
    );
    let repo = grit_repo(consumer.path());
    let bundle = Bundle::open(&bundle_path).expect("open");
    assert_eq!(
        bundle.header().object_format,
        grit_lib::objects::HashAlgo::Sha256
    );
    assert!(bundle.verify(&repo).expect("verify").is_ok());
    let refs = bundle.unbundle(&repo).expect("unbundle");
    for r in &refs {
        git_ok(consumer.path(), &["update-ref", &r.name, &r.oid.to_hex()]);
    }
    fsck_strict(consumer.path());
}

#[test]
fn git_v3_filter_bundle_unbundles_with_promisor_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path();
    git_ok(src, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("big"), vec![b'a'; 4096]).unwrap();
    git_ok(src, &["add", "big"]);
    git_ok(src, &["commit", "-q", "-m", "blob"]);
    let bundle_path = src.join("filtered.bundle");
    let filter_try = Command::new("git")
        .current_dir(src)
        .args([
            "bundle",
            "create",
            "--filter=blob:none",
            bundle_path.to_str().unwrap(),
            "HEAD",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("filter bundle create");
    if !filter_try.status.success() {
        eprintln!("SKIP: git bundle create --filter not supported");
        return;
    }

    let consumer = tempfile::tempdir().expect("consumer");
    git_ok(
        consumer.path(),
        &["init", "-q", "--bare", "-b", "main", "."],
    );
    let repo = grit_repo(consumer.path());
    let bundle = Bundle::open(&bundle_path).expect("open");
    assert!(bundle.header().filter.is_some());
    let refs = bundle.unbundle(&repo).expect("unbundle");
    for r in &refs {
        git_ok(consumer.path(), &["update-ref", &r.name, &r.oid.to_hex()]);
    }
    let promisor = std::fs::read_dir(consumer.path().join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "promisor"));
    assert!(
        promisor.is_some(),
        "filter bundle ingest should mark promisor pack"
    );
    fsck_strict(consumer.path());
}

#[test]
fn verify_prerequisites_not_connected_when_dangling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path();
    let (c1, c2) = init_repo_with_two_commits(src);
    let inc = src.join("inc.bundle");
    git_ok(
        src,
        &[
            "bundle",
            "create",
            inc.to_str().unwrap(),
            &format!("{c1}..{c2}"),
            "main",
        ],
    );

    let consumer = tempfile::tempdir().expect("consumer");
    git_ok(
        consumer.path(),
        &["init", "-q", "--bare", "-b", "main", "."],
    );
    // Loose prerequisite object without refs.
    let obj_path = src
        .join(".git/objects")
        .join(&c1.to_hex()[..2])
        .join(&c1.to_hex()[2..]);
    let dest_dir = consumer.path().join("objects").join(&c1.to_hex()[..2]);
    std::fs::create_dir_all(&dest_dir).expect("dest dir");
    std::fs::copy(&obj_path, dest_dir.join(&c1.to_hex()[2..])).expect("copy loose");

    let repo = grit_repo(consumer.path());
    let bundle = Bundle::open(&inc).expect("open");
    let report = bundle.verify(&repo).expect("verify");
    assert!(report.missing_prerequisites.is_empty());
    assert!(report.prerequisites_not_connected);
    assert_eq!(
        report.into_result().unwrap_err(),
        BundleError::PrerequisitesNotConnected
    );
}

#[test]
fn malformed_header_errors() {
    let bad_sig = tempfile::NamedTempFile::new().expect("tmp");
    std::fs::write(bad_sig.path(), b"not a bundle\n").unwrap();
    assert_eq!(
        Bundle::open(bad_sig.path()).unwrap_err(),
        BundleError::BadSignature
    );

    let oid = "b".repeat(40);
    let truncated = format!("# v2 git bundle\n{oid}\n\nPACK");
    let mut cur = std::io::Cursor::new(truncated.into_bytes());
    assert!(matches!(
        read_header(&mut cur).unwrap_err(),
        BundleError::MalformedLine(_)
    ));

    let cap = b"# v3 git bundle\n@object-format=sha256\n@filter=blob:none\n\nPACK";
    let mut cur = std::io::Cursor::new(cap.as_slice());
    let header = read_header(&mut cur).expect("v3 cap header");
    assert_eq!(header.object_format, grit_lib::objects::HashAlgo::Sha256);
}
