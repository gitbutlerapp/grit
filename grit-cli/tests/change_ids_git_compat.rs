//! End-to-end change-id workflow with the `grit` CLI and system `git`.

use std::collections::HashMap;
use std::fs;
use std::process::Command;

use grit_lib::change_id::{change_id, resolved_change_id, ChangeId};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;
use grit_test_support::{git, grit, unique_tmp};
use serde_json::Value;

fn grit_json(dir: &std::path::Path, args: &[&str]) -> Value {
    let out = grit(dir, args);
    assert!(out.ok(), "grit {:?} failed: {}", args, out.stderr);
    serde_json::from_str(&out.stdout).expect("json stdout")
}

fn change_id_from_cat(cat: &str) -> Option<String> {
    cat.lines()
        .find(|l| l.starts_with("change-id "))
        .map(|l| l.strip_prefix("change-id ").unwrap().to_owned())
}

fn init_repo(dir: &std::path::Path) {
    assert!(grit(dir, &["init"]).ok(), "grit init");
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@e.com"]);
}

fn commit_file(dir: &std::path::Path, path: &str, body: &str, msg: &str) -> Value {
    fs::write(dir.join(path), body).expect("write");
    grit_json(dir, &["commit", "-m", msg, "--json"])
}

fn change_id_of_commit(dir: &std::path::Path, oid: &str) -> String {
    let cat = git(dir, &["cat-file", "-p", oid]);
    change_id_from_cat(&cat).unwrap_or_else(|| panic!("change-id missing on {oid}:\n{cat}"))
}

fn logical_id_from_log(dir: &std::path::Path, oid: &str) -> (String, bool) {
    let show = grit_json(dir, &["show", oid, "--json"]);
    let commit = &show["commit"];
    (
        commit["change_id"].as_str().expect("change_id").to_owned(),
        commit["change_id_stored"]
            .as_bool()
            .expect("change_id_stored"),
    )
}

fn rewritten_row<'a>(json: &'a Value, old: &str) -> &'a Value {
    json["rewritten"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["old"].as_str() == Some(old)))
        .unwrap_or_else(|| panic!("rewritten row for {old} in {json}"))
}

fn git_trailer_change_id(dir: &std::path::Path, oid: &str) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["log", "-1", oid, "--format=%(trailers:unfold)"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git log");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|line| {
        line.strip_prefix("change-id: ")
            .map(str::trim)
            .map(str::to_owned)
    })
}

#[test]
fn change_ids_end_to_end_with_system_git() {
    let dir = unique_tmp("change-ids-e2e", "git-compat");
    init_repo(&dir);

    let c1 = commit_file(&dir, "a.txt", "a\n", "one");
    let id1 = c1["change_id"].as_str().expect("id1").to_owned();
    assert_eq!(
        change_id_of_commit(&dir, c1["oid"].as_str().expect("c1 oid")),
        id1
    );

    let c2 = commit_file(&dir, "b.txt", "b\n", "two");
    let oid2 = c2["oid"].as_str().expect("oid2").to_owned();
    let id2 = c2["change_id"].as_str().expect("id2").to_owned();

    commit_file(&dir, "c.txt", "c\n", "three");
    let c4 = commit_file(&dir, "d.txt", "d\n", "four");
    let oid4 = c4["oid"].as_str().expect("oid4").to_owned();
    let id4 = c4["change_id"].as_str().expect("id4").to_owned();

    let amended = grit_json(&dir, &["commit", "--amend", "-m", "four amended", "--json"]);
    assert_eq!(amended["change_id"].as_str(), Some(id4.as_str()));
    assert_ne!(amended["oid"].as_str(), Some(oid4.as_str()));

    let reword = grit_json(
        &dir,
        &["history", "reword", &oid2, "-m", "two reworded", "--json"],
    );
    let row = rewritten_row(&reword, &oid2);
    assert_eq!(row["change_id"].as_str(), Some(id2.as_str()));
    let new_oid2 = row["new"].as_str().expect("new oid2");
    assert_eq!(change_id_of_commit(&dir, new_oid2), id2);

    fs::write(dir.join("b.txt"), "b\nextra\n").expect("stage fixup");
    assert!(grit(&dir, &["add", "b.txt"]).ok());
    let fixup = grit_json(&dir, &["history", "fixup", new_oid2, "--json"]);
    let fixup_row = rewritten_row(&fixup, new_oid2);
    assert_eq!(fixup_row["change_id"].as_str(), Some(id2.as_str()));

    let tip = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    let base = git(&dir, &["rev-parse", "HEAD~2"]).trim().to_owned();
    let squash_target = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_owned();
    let id_squash_target = change_id_of_commit(&dir, &squash_target);
    let range = format!("{base}..{tip}");
    let squash = grit_json(
        &dir,
        &[
            "history",
            "squash",
            &range,
            "-m",
            "squashed tip",
            "--update-refs",
            "head",
            "--json",
        ],
    );
    for row in squash["rewritten"].as_array().expect("rewritten") {
        if row["new"].is_string() {
            assert_eq!(row["change_id"].as_str(), Some(id_squash_target.as_str()));
        }
    }

    commit_file(&dir, "e1.txt", "e1\n", "drop chain 1");
    let drop2 = commit_file(&dir, "e2.txt", "e2\n", "drop chain 2");
    let oid_drop2 = drop2["oid"].as_str().expect("drop2").to_owned();
    let drop3 = commit_file(&dir, "e3.txt", "e3\n", "drop chain 3");
    let oid_drop3 = drop3["oid"].as_str().expect("drop3").to_owned();
    let id_drop3 = drop3["change_id"].as_str().expect("id drop3").to_owned();
    let id_drop1 = change_id_of_commit(&dir, git(&dir, &["rev-parse", "HEAD~2"]).trim());

    let drop = grit_json(&dir, &["history", "drop", &oid_drop2, "--json"]);
    assert!(drop["rewritten"]
        .as_array()
        .expect("rewritten")
        .iter()
        .any(|r| r["old"].as_str() == Some(oid_drop2.as_str())));
    let new_drop3 = rewritten_row(&drop, &oid_drop3)["new"]
        .as_str()
        .expect("new drop3");
    assert_eq!(change_id_of_commit(&dir, new_drop3), id_drop3);
    let parent_after_drop = git(&dir, &["rev-parse", "HEAD~1"]);
    assert_eq!(
        change_id_of_commit(&dir, parent_after_drop.trim()),
        id_drop1
    );

    assert!(grit(&dir, &["switch", "-c", "side"]).ok());
    commit_file(&dir, "side.txt", "side\n", "side work");
    let side_oid = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    let side_id = change_id_of_commit(&dir, &side_oid);
    assert!(grit(&dir, &["switch", "main"]).ok());
    commit_file(&dir, "main.txt", "main\n", "main diverge");
    let pick = grit_json(&dir, &["pick", &side_oid, "--json"]);
    let pick_id = pick["change_id"].as_str().expect("pick id");
    assert_ne!(pick_id, side_id.as_str());
    assert_eq!(change_id_of_commit(&dir, "HEAD"), pick_id);

    let revert_target = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_owned();
    let revert_target_id = change_id_of_commit(&dir, &revert_target);
    let revert = grit_json(&dir, &["revert", &revert_target, "--json"]);
    let revert_id = revert["change_id"].as_str().expect("revert id");
    assert_ne!(revert_id, revert_target_id.as_str());

    let prefix = id1.chars().take(8).collect::<String>();
    let resolved = grit_json(&dir, &["show", &prefix, "--json"]);
    assert_eq!(
        resolved["commit"]["oid"].as_str(),
        Some(c1["oid"].as_str().expect("c1 oid"))
    );

    let clone_parent = dir.parent().expect("parent").join("clone.git");
    let clone_dir = dir.parent().expect("parent").join("clone-wt");
    if clone_parent.exists() {
        fs::remove_dir_all(&clone_parent).ok();
    }
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).ok();
    }
    git(
        dir.parent().expect("parent"),
        &[
            "clone",
            dir.to_str().expect("path"),
            clone_dir.to_str().expect("clone"),
        ],
    );
    git(&clone_dir, &["fsck", "--strict"]);

    let mut ids_on_clone = HashMap::new();
    for oid in git(&clone_dir, &["rev-list", "--all"])
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let cat = git(&clone_dir, &["cat-file", "-p", oid]);
        if let Some(id) = change_id_from_cat(&cat) {
            ids_on_clone.insert(oid.to_owned(), id.clone());
            assert_eq!(
                git_trailer_change_id(&clone_dir, oid).as_deref(),
                None,
                "change-id lives in commit headers; git trailers stay empty"
            );
            assert_eq!(change_id_of_commit(&clone_dir, oid), id);
        }
    }
    assert!(
        ids_on_clone.len() >= 5,
        "expected stored change ids on clone"
    );

    fs::write(clone_dir.join("git-only.txt"), "git\n").expect("write");
    git(&clone_dir, &["add", "git-only.txt"]);
    git(&clone_dir, &["commit", "-m", "git-only"]);
    let headerless = git(&clone_dir, &["rev-parse", "HEAD"]).trim().to_owned();
    let derived =
        ChangeId::derived_from_commit_id(&headerless.parse::<ObjectId>().expect("parse oid"));
    let (before_stored, before_flag) = logical_id_from_log(&clone_dir, &headerless);
    assert!(!before_flag);
    assert_eq!(before_stored, derived.as_str());

    let reword_git = grit_json(
        &clone_dir,
        &[
            "history",
            "reword",
            &headerless,
            "-m",
            "git-only reworded",
            "--json",
        ],
    );
    let row = rewritten_row(&reword_git, &headerless);
    assert_eq!(row["change_id"].as_str(), Some(derived.as_str()));
    let stored_oid = row["new"].as_str().expect("stored oid");
    assert_eq!(
        change_id_of_commit(&clone_dir, stored_oid),
        derived.as_str()
    );
    let (_, stored_flag) = logical_id_from_log(&clone_dir, stored_oid);
    assert!(stored_flag);

    let disabled_dir = dir.parent().expect("parent").join("change-ids-off");
    if disabled_dir.exists() {
        fs::remove_dir_all(&disabled_dir).ok();
    }
    fs::create_dir_all(&disabled_dir).expect("mkdir");
    init_repo(&disabled_dir);
    git(&disabled_dir, &["config", "grit.changeIds", "false"]);
    let off_commit = commit_file(&disabled_dir, "off.txt", "off\n", "no header");
    assert!(off_commit["change_id"].is_null());
    let cat = git(&disabled_dir, &["cat-file", "-p", "HEAD"]);
    assert!(
        change_id_from_cat(&cat).is_none(),
        "grit.changeIds=false must not add headers:\n{cat}"
    );
    fs::write(disabled_dir.join("off.txt"), "off2\n").expect("write");
    let off_amend = grit_json(
        &disabled_dir,
        &["commit", "--amend", "-m", "still no header", "--json"],
    );
    assert!(off_amend["change_id"].is_null());
    let (_, stored_flag) = logical_id_from_log(&disabled_dir, "HEAD");
    assert!(!stored_flag);
    let cat2 = git(&disabled_dir, &["cat-file", "-p", "HEAD"]);
    assert!(change_id_from_cat(&cat2).is_none());

    let repo = Repository::discover(Some(&dir), &RepositoryOptions::empty()).expect("open");
    let head = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    let data = parse_commit(
        &repo
            .odb()
            .read(&head.parse().expect("oid"))
            .expect("read")
            .data,
    )
    .expect("parse");
    assert!(change_id(&data).is_some());
    let _ = resolved_change_id(&head.parse().expect("oid"), &data);

    git(&dir, &["fsck", "--strict"]);
    git(&clone_dir, &["fsck", "--strict"]);
}
