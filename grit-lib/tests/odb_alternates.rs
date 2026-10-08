//! Object database alternates (`info/alternates`, env overrides) — t5613/t5615/t1060 object-level scenarios.

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::read_alternates_recursive;
use grit_test_support::git_cmd;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_alternate_lines(git_dir: &Path) -> Vec<PathBuf> {
    let out = Command::new("git")
        .args(["count-objects", "-v"])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count-objects");
    assert!(
        out.status.success(),
        "git count-objects -v: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("alternate: "))
        .map(|p| fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p)))
        .collect()
}

fn assert_alternate_order_matches_git(objects_dir: &Path, git_dir: &Path, label: &str) {
    let grit: Vec<PathBuf> = read_alternates_recursive(objects_dir)
        .unwrap_or_else(|e| panic!("{label}: grit alternates: {e}"))
        .into_iter()
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .collect();
    let git = git_alternate_lines(git_dir);
    assert_eq!(
        grit, git,
        "{label}: alternates order must match git count-objects -v"
    );
}

fn init_primary_with_alternates_file(objects_dir: &Path, body: &str) -> PathBuf {
    fs::create_dir_all(objects_dir.join("info")).expect("info dir");
    fs::write(objects_dir.join("info/alternates"), body).expect("alternates");
    objects_dir.to_path_buf()
}

#[test]
fn alternates_absolute_relative_comments_and_blank_lines() {
    let layout = TempDir::new().expect("tempdir");
    let alt = layout.path().join("alt-store/objects");
    fs::create_dir_all(&alt).expect("alt objects");
    let alt_odb = Odb::new(&alt);
    let oid = alt_odb
        .write(ObjectKind::Blob, b"from-alternate")
        .expect("alt write");

    let primary_git = layout.path().join("primary.git");
    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    let primary_objects = primary_git.join("objects");
    let alt_abs = fs::canonicalize(&alt).expect("canonical alt");
    init_primary_with_alternates_file(
        &primary_objects,
        &format!(
            "# comment line\n\n{}\n# trailing comment\n",
            alt_abs.display()
        ),
    );

    assert_alternate_order_matches_git(&primary_objects, &primary_git, "abs+comments");
    let odb = Odb::new(&primary_objects);
    assert!(odb.exists(&oid));
    assert!(odb.read(&oid).is_ok());
    assert!(odb.read_info(&oid).is_ok());
    assert!(!odb.exists_local(&oid));
    let local_oid = odb
        .write_local(ObjectKind::Blob, b"local-only")
        .expect("local");
    assert!(odb.exists_local(&local_oid));
    assert!(primary_objects.join("info/alternates").is_file());
    let hex = local_oid.to_hex();
    assert!(
        !alt.join(&hex[0..2]).join(&hex[2..]).exists(),
        "write_local must not land in alternate"
    );

    // Relative entry from a non-bare layout (t1060-style read from alternate).
    let rel_layout = TempDir::new().expect("rel layout");
    fs::create_dir_all(rel_layout.path().join("alt/objects")).expect("rel alt");
    let rel_alt = rel_layout.path().join("alt/objects");
    let rel_alt_odb = Odb::new(&rel_alt);
    let rel_oid = rel_alt_odb
        .write(ObjectKind::Blob, b"relative-alt")
        .expect("rel alt write");
    let rel_primary = rel_layout.path().join("repo/objects");
    init_primary_with_alternates_file(&rel_primary, "../../alt/objects\n");
    let rel_odb = Odb::with_work_tree(&rel_primary, rel_layout.path().join("repo").as_path());
    assert!(rel_odb.exists(&rel_oid));
    assert!(!rel_odb.exists_local(&rel_oid));
}

#[test]
fn alternates_quoted_path_with_spaces() {
    let layout = TempDir::new().expect("tempdir");
    let alt = layout.path().join("alt with spaces/objects");
    fs::create_dir_all(&alt).expect("quoted alt");
    let alt_odb = Odb::new(&alt);
    let oid = alt_odb
        .write(ObjectKind::Blob, b"quoted-path")
        .expect("write");

    let primary_git = layout.path().join("primary.git");
    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    let primary_objects = primary_git.join("objects");
    let escaped = alt
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    init_primary_with_alternates_file(&primary_objects, &format!("\"{escaped}\"\n"));

    assert_alternate_order_matches_git(&primary_objects, &primary_git, "quoted");
    let odb = Odb::new(&primary_objects);
    assert!(odb.exists(&oid));
}

#[test]
fn alternates_missing_directory_is_skipped() {
    let layout = TempDir::new().expect("tempdir");
    let primary_git = layout.path().join("primary.git");
    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    let primary_objects = primary_git.join("objects");
    init_primary_with_alternates_file(&primary_objects, "/nonexistent/alternate/objects\n");
    let resolved = read_alternates_recursive(&primary_objects).expect("read");
    assert_eq!(resolved.len(), 1);
    let odb = Odb::new(&primary_objects);
    let missing = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").expect("oid");
    assert!(!odb.exists(&missing));
}

#[test]
fn alternates_chain_a_b_c_order_matches_git() {
    let layout = TempDir::new().expect("tempdir");
    let store_c = layout.path().join("store-c/objects");
    let store_b = layout.path().join("store-b/objects");
    let store_a = layout.path().join("store-a/objects");
    for p in [&store_c, &store_b, &store_a] {
        fs::create_dir_all(p.join("info")).expect("mkdir");
    }
    fs::write(store_c.join("info/alternates"), b"").ok();
    fs::write(
        store_b.join("info/alternates"),
        format!("{}\n", fs::canonicalize(&store_c).unwrap().display()),
    )
    .expect("b->c");
    fs::write(
        store_a.join("info/alternates"),
        format!("{}\n", fs::canonicalize(&store_b).unwrap().display()),
    )
    .expect("a->b");

    let primary_git = layout.path().join("primary.git");
    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    let primary_objects = primary_git.join("objects");
    init_primary_with_alternates_file(
        &primary_objects,
        &format!("{}\n", fs::canonicalize(&store_a).unwrap().display()),
    );

    assert_alternate_order_matches_git(&primary_objects, &primary_git, "chain");

    let blob_odb = Odb::new(&store_c);
    let oid = blob_odb
        .write(ObjectKind::Blob, b"deep-chain")
        .expect("write");
    let odb = Odb::new(&primary_objects);
    assert!(odb.exists(&oid));
    assert!(odb.read(&oid).is_ok());
}

#[test]
fn alternates_cycle_and_self_reference_finish_without_hang() {
    let layout = TempDir::new().expect("tempdir");
    let a = layout.path().join("a/objects");
    let b = layout.path().join("b/objects");
    fs::create_dir_all(a.join("info")).expect("a info");
    fs::create_dir_all(b.join("info")).expect("b info");
    let a_canon = fs::canonicalize(&a).unwrap();
    let b_canon = fs::canonicalize(&b).unwrap();
    fs::write(
        a.join("info/alternates"),
        format!("{}\n", b_canon.display()),
    )
    .expect("a->b");
    fs::write(
        b.join("info/alternates"),
        format!("{}\n", a_canon.display()),
    )
    .expect("b->a");

    let start = Instant::now();
    let _ = read_alternates_recursive(&a).expect("cycle a");
    let _ = read_alternates_recursive(&b).expect("cycle b");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "alternates cycle must finish quickly"
    );

    let cycle_git = layout.path().join("cycle.git");
    git_in(layout.path(), &["init", "-q", "--bare", "cycle.git"]);
    init_primary_with_alternates_file(
        &cycle_git.join("objects"),
        &format!("{}\n", a_canon.display()),
    );
    assert_alternate_order_matches_git(&cycle_git.join("objects"), &cycle_git, "cycle");

    fs::write(
        a.join("info/alternates"),
        format!("{}\n", a_canon.display()),
    )
    .expect("self");
    let self_git = layout.path().join("self.git");
    git_in(layout.path(), &["init", "-q", "--bare", "self.git"]);
    init_primary_with_alternates_file(
        &self_git.join("objects"),
        &format!("{}\n", a_canon.display()),
    );
    assert_alternate_order_matches_git(&self_git.join("objects"), &self_git, "self-ref");
}

#[test]
fn alternates_recursion_depth_limit_matches_git() {
    let layout = TempDir::new().expect("tempdir");
    let mut dirs = Vec::new();
    for i in 0..8 {
        let p = layout.path().join(format!("level-{i}/objects"));
        fs::create_dir_all(p.join("info")).expect("info");
        dirs.push(p);
    }
    for i in 0..dirs.len() - 1 {
        let next = fs::canonicalize(&dirs[i + 1]).unwrap();
        fs::write(
            dirs[i].join("info/alternates"),
            format!("{}\n", next.display()),
        )
        .expect("link");
    }
    let primary_git = layout.path().join("primary.git");
    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    let primary_objects = primary_git.join("objects");
    let first = fs::canonicalize(&dirs[0]).unwrap();
    init_primary_with_alternates_file(&primary_objects, &format!("{}\n", first.display()));
    assert_alternate_order_matches_git(&primary_objects, &primary_git, "depth");
}

#[test]
fn alternates_append_and_cache_refresh() {
    let layout = TempDir::new().expect("tempdir");
    let alt = layout.path().join("alt/objects");
    fs::create_dir_all(&alt).expect("alt");
    let alt_odb = Odb::new(&alt);
    let oid = alt_odb.write(ObjectKind::Blob, b"late-alt").expect("write");

    let primary_objects = layout.path().join("primary/objects");
    fs::create_dir_all(primary_objects.join("info")).expect("info");
    let odb = Odb::new(&primary_objects);
    assert!(!odb.exists(&oid));
    odb.append_file_alternate(&alt).expect("append");
    assert!(odb.exists(&oid));

    fs::write(primary_objects.join("info/alternates"), b"").expect("external clear");
    assert!(
        odb.exists(&oid),
        "cached alternates chain may linger until refresh"
    );
    odb.refresh_file_alternates_from_disk()
        .expect("refresh from disk");
    assert!(!odb.exists(&oid));
    odb.append_file_alternate(&alt).expect("re-append");
    assert!(odb.exists(&oid));

    let alt2 = layout.path().join("alt2/objects");
    fs::create_dir_all(&alt2).expect("alt2");
    Odb::append_alternate_objects_line(&primary_objects, &alt2).expect("append line only");
    odb.refresh_file_alternates_from_disk().expect("refresh");
    let chain = read_alternates_recursive(&primary_objects).expect("chain");
    assert!(chain
        .iter()
        .any(|p| fs::canonicalize(p).unwrap_or(p.clone()) == fs::canonicalize(&alt2).unwrap()));
}

#[test]
fn alternates_env_dirs_explicit_match_git_reachability() {
    let layout = TempDir::new().expect("tempdir");
    let alt = layout.path().join("alt/objects");
    fs::create_dir_all(&alt).expect("alt");
    let alt_odb = Odb::new(&alt);
    let oid = alt_odb.write(ObjectKind::Blob, b"env-alt").expect("write");

    let primary_objects = layout.path().join("primary/objects");
    fs::create_dir_all(&primary_objects).expect("primary");
    let odb = Odb::new(&primary_objects).with_env_alternate_dirs(vec![alt.clone()]);
    assert!(odb.exists(&oid));
    assert!(!odb.exists_local(&oid));

    let primary_git = layout.path().join("primary.git");
    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    fs::create_dir_all(primary_git.join("objects")).expect("objects");
    let hex = oid.to_hex();
    let cat = Command::new("git")
        .args(["cat-file", "-e", &hex])
        .env("GIT_DIR", &primary_git)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &alt)
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git cat-file");
    assert!(
        cat.status.success(),
        "git must see env alternate object: {}",
        String::from_utf8_lossy(&cat.stderr)
    );
}

#[test]
fn alternates_corrupt_local_loose_readable_from_alternate() {
    let layout = TempDir::new().expect("tempdir");
    let alt_bare = layout.path().join("alt.git");
    git_in(layout.path(), &["init", "-q", "--bare", "alt.git"]);
    let alt_objects = alt_bare.join("objects");
    let wt = layout.path().join("wt");
    git_in(layout.path(), &["init", "-q", "-b", "main", "wt"]);
    git_in(&wt, &["config", "user.email", "t@example.com"]);
    git_in(&wt, &["config", "user.name", "T"]);
    git_in(&wt, &["config", "core.untrackedCache", "false"]);
    fs::write(wt.join("f"), b"x").expect("file");
    git_in(&wt, &["add", "f"]);
    git_in(&wt, &["commit", "-m", "c", "-q"]);
    git_in(
        &wt,
        &[
            "push",
            "-q",
            alt_bare.to_str().unwrap(),
            "HEAD:refs/heads/main",
        ],
    );
    let commit_hex = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(&wt)
        .exec()
        .stdout
        .trim()
        .to_string();
    let commit = ObjectId::from_hex(&commit_hex).expect("oid");

    let primary_objects = layout.path().join("primary/objects");
    init_primary_with_alternates_file(
        &primary_objects,
        &format!("{}\n", fs::canonicalize(&alt_objects).unwrap().display()),
    );
    let loose = Odb::new(&primary_objects).object_path(&commit);
    fs::create_dir_all(loose.parent().unwrap()).expect("dir");
    fs::write(&loose, b"corrupt-not-a-git-object").expect("corrupt");

    let odb = Odb::new(&primary_objects);
    assert!(odb.exists(&commit), "alternate copy satisfies exists");
    let obj = odb
        .read(&commit)
        .expect("read via alternate after corrupt local");
    assert_eq!(obj.kind, ObjectKind::Commit);
    assert!(odb.read_info(&commit).is_ok());
}

#[test]
fn alternates_write_never_targets_alternate_store() {
    let layout = TempDir::new().expect("tempdir");
    let alt = layout.path().join("alt/objects");
    fs::create_dir_all(&alt).expect("alt");
    let primary_objects = layout.path().join("primary/objects");
    init_primary_with_alternates_file(
        &primary_objects,
        &format!("{}\n", fs::canonicalize(&alt).unwrap().display()),
    );
    let odb = Odb::new(&primary_objects);
    let payload = b"primary write";
    let oid = odb.write(ObjectKind::Blob, payload).expect("write");
    assert!(
        primary_objects
            .join(&oid.to_hex()[0..2])
            .join(&oid.to_hex()[2..])
            .is_file(),
        "write lands under primary objects"
    );
    assert!(!alt
        .join(&oid.to_hex()[0..2])
        .join(&oid.to_hex()[2..])
        .exists());
    let oid2 = odb.write_local(ObjectKind::Blob, b"other").expect("local");
    assert!(odb.object_path(&oid2).is_file());
    let hex2 = oid2.to_hex();
    assert!(!alt.join(&hex2[0..2]).join(&hex2[2..]).exists());
}
