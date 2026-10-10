//! Abbreviation lookup via [`Odb::lookup_prefix`] matches `git rev-parse --disambiguate`.

use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::list_all_abbrev_matches;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_disambiguate(repo_dir: &Path, prefix: &str) -> Vec<String> {
    let spec = format!("--disambiguate={prefix}");
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["rev-parse", &spec])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git disambiguate");
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn lookup_prefix_matches_git_disambiguate_loose_pack_midx_and_alternate() {
    let root = TempDir::new().expect("tempdir");
    let primary = root.path().join("primary");
    let alt = root.path().join("alt");
    fs::create_dir_all(&primary).expect("primary");
    fs::create_dir_all(&alt).expect("alt");

    git(&primary, &["init"]);
    git(&primary, &["config", "core.multiPackIndex", "true"]);
    fs::write(
        primary.join("loose-only.txt"),
        b"loose blob for prefix tests",
    )
    .expect("write");
    git(&primary, &["add", "loose-only.txt"]);
    git(&primary, &["commit", "-m", "loose"]);
    git(&primary, &["branch", "packed"]);
    fs::write(primary.join("packed.txt"), b"packed blob payload").expect("write");
    git(&primary, &["add", "packed.txt"]);
    git(&primary, &["commit", "-m", "packed"]);
    git(&primary, &["repack", "-a", "-d"]);
    git(&primary, &["multi-pack-index", "write"]);

    git(&alt, &["init"]);
    fs::write(alt.join("alt.txt"), b"alternate-only object").expect("write");
    git(&alt, &["add", "alt.txt"]);
    git(&alt, &["commit", "-m", "alt"]);
    let alt_objects = alt.join(".git/objects");
    fs::create_dir_all(primary.join(".git/objects/info")).expect("info");
    fs::write(
        primary.join(".git/objects/info/alternates"),
        format!("{}\n", alt_objects.display()),
    )
    .expect("alternates");

    let git_dir = primary.join(".git");
    let repo = Repository::open(git_dir.as_path(), Some(&primary)).expect("open");
    let odb = &repo.odb;

    // Sanity: objects span loose, pack, MIDX, and alternate layers.
    let mut all = Vec::new();
    odb.for_each_object(&mut |oid| {
        all.push(oid.to_hex());
        std::ops::ControlFlow::Continue(())
    })
    .expect("for_each");
    assert!(all.len() >= 4, "expected layered object set, got {all:?}");

    let alt_head = git(&alt, &["rev-parse", "HEAD"]);
    let alt_oid: ObjectId = alt_head.parse().expect("alt oid");
    assert!(
        odb.exists(&alt_oid),
        "alternate object must resolve through composed Odb"
    );

    let prefixes: Vec<String> = all
        .iter()
        .flat_map(|hex| (4..=7).map(move |n| hex[..n].to_owned()))
        .collect();

    for prefix in prefixes {
        let mut grit_via_odb = Vec::new();
        odb.lookup_prefix(&prefix, 0, &mut grit_via_odb)
            .expect("lookup_prefix");
        grit_via_odb.sort_by_key(|o| o.to_hex());

        let grit_via_rev_parse = list_all_abbrev_matches(&repo, &prefix).expect("rev_parse");
        let mut grit_rev_hex: Vec<String> = grit_via_rev_parse.iter().map(|o| o.to_hex()).collect();
        grit_rev_hex.sort();

        let git_matches = git_disambiguate(&primary, &prefix);
        let mut git_sorted = git_matches.clone();
        git_sorted.sort();

        let grit_odb_hex: Vec<String> = grit_via_odb.iter().map(|o| o.to_hex()).collect();
        assert_eq!(
            grit_odb_hex, git_sorted,
            "Odb::lookup_prefix mismatch for `{prefix}` (git {git_sorted:?})"
        );
        assert_eq!(
            grit_rev_hex, git_sorted,
            "list_all_abbrev_matches mismatch for `{prefix}` (git {git_sorted:?})"
        );
    }

    let _ = git_dir;
}
