//! Annotated tag creation round-trips with system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::maintenance::GcOptions;
use grit_lib::objects::{parse_tag, ObjectKind};
use grit_lib::porcelain::tag::{tag_ref_metadata, CreateTagRequest, TagDetail, TagKind};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::listing::RefQuery;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::repo::Repository;
use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn grit_annotated_tag_round_trips_with_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "x\n").unwrap();
    git(&root, &["add", "f"]);
    git(&root, &["commit", "-qm", "seed"]);

    let repo =
        Repository::discover(Some(root.as_path()), &RepositoryOptions::empty()).expect("discover");
    let head = repo.references().resolve("HEAD").unwrap();
    let tagger = "Tagger <t@example.com> 946684800 +0000";
    let outcome = repo
        .tags()
        .create(
            &CreateTagRequest::new("v1".to_owned(), head)
                .with_message(Some("release\n".to_owned()))
                .with_tagger(Some(tagger.to_owned())),
        )
        .expect("create annotated tag");
    assert_eq!(outcome.kind, TagKind::Annotated);
    assert_eq!(outcome.peeled, Some(head));

    git(&root, &["fsck"]);
    let cat = git(&root, &["cat-file", "-p", "v1"]);
    assert!(cat.contains("object "));
    assert!(cat.contains("tag v1"));
    assert!(cat.contains("release"));

    let (kind, peeled) = tag_ref_metadata(&repo, &outcome.oid);
    assert_eq!(kind, TagKind::Annotated);
    assert_eq!(peeled, Some(head));

    let obj = repo.odb().read(&outcome.oid).unwrap();
    assert_eq!(obj.kind, ObjectKind::Tag);
    let parsed = parse_tag(&obj.data).unwrap();
    assert_eq!(parsed.tag, "v1");
    assert_eq!(parsed.object, head);
}

#[test]
fn git_non_utf8_annotated_tag_message_is_not_corrupt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let host = tmp.path().join("host");
    std::fs::create_dir_all(&host).unwrap();
    git(&host, &["init", "-q", "-b", "main"]);
    git(&host, &["config", "user.email", "a@b"]);
    git(&host, &["config", "user.name", "T"]);
    std::fs::write(host.join("f"), "1\n").unwrap();
    git(&host, &["add", "f"]);
    git(&host, &["commit", "-qm", "init"]);
    let tag_out = Command::new("bash")
        .current_dir(&host)
        .args(["-c", "git tag -a -m $'release caf\\xe9' v1"])
        .output()
        .expect("git tag");
    assert!(
        tag_out.status.success(),
        "git tag: {}",
        String::from_utf8_lossy(&tag_out.stderr)
    );
    git(&host, &["fsck"]);

    let host_repo =
        Repository::discover(Some(host.as_path()), &RepositoryOptions::empty()).expect("discover");
    let fsck_opts = FsckOptions::full_repository(false).with_check_dangling(false);
    let report = host_repo
        .maintenance()
        .fsck(&fsck_opts, &mut NullProgress, &CancelToken::never())
        .expect("grit fsck");
    let config = host_repo.config().snapshot().expect("config");
    assert!(
        report.ok(&config, &fsck_opts),
        "grit fsck failed: {:?}",
        report.issues
    );

    let listed = host_repo
        .tags()
        .list(
            &grit_lib::repo_handles::TagListOptions::default()
                .with_query(RefQuery::default())
                .with_detail(TagDetail::Annotation),
        )
        .expect("list tags");
    let grit_lib::porcelain::tag::TagList::Summaries(tags) = listed else {
        panic!("expected annotation summaries");
    };
    assert_eq!(tags.len(), 1);
    let ann = tags[0].annotation.as_ref().expect("annotation");
    assert!(ann.message.contains('\u{FFFD}'));

    host_repo
        .maintenance()
        .gc(
            &GcOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("grit gc");

    let clone_dest = tmp.path().join("clone");
    clone_repo(
        CloneOptions::new(format!("file://{}", host.display()), clone_dest.clone()),
        &mut NoProgress,
        &CancelToken::never(),
        None,
    )
    .expect("grit clone");
    let cloned = Repository::discover(Some(clone_dest.as_path()), &RepositoryOptions::empty())
        .expect("discover clone");
    let clone_report = cloned
        .maintenance()
        .fsck(&fsck_opts, &mut NullProgress, &CancelToken::never())
        .expect("grit fsck clone");
    let clone_config = cloned.config().snapshot().expect("config");
    assert!(
        clone_report.ok(&clone_config, &fsck_opts),
        "clone fsck: {:?}",
        clone_report.issues
    );
}
