//! A push whose tracking name is a filesystem path (issue #2556): the remote ref
//! lands, the push succeeds, and no `refs/remotes/<path>/…` ref is written, as with
//! `git push <path> <branch>`. The remote is checked with the system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
use grit_lib::remote::Remote;
use grit_lib::repo::Repository;
use grit_test_support::{git, rev_parse};

#[test]
fn push_with_backslash_path_tracking_name_succeeds_without_tracking_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("a.txt"), "hi\n").expect("write");
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-q", "-m", "one"]);
    let tip = rev_parse(&work, "refs/heads/main");

    let name = r"C:\t\remote.git";
    let bare = tmp.path().join(name);
    git(tmp.path(), &["init", "-q", "--bare", name]);

    let repo = Repository::discover(Some(&work), &RepositoryOptions::empty()).expect("open work");
    let remote = Remote::from_url(bare.to_str().expect("utf8")).expect("url");
    let spec = PushRefSpec::default()
        .with_src(Some(tip))
        .with_src_ref(Some("refs/heads/main".to_owned()))
        .with_dst("refs/heads/main".to_owned());
    let outcome = remote
        .push(
            &repo,
            &[spec],
            PushOptions::default().with_tracking_remote(Some(name.to_owned())),
            &mut NullProgress,
            &CancelToken::never(),
            None,
        )
        .expect("push to a backslash path succeeds");
    let statuses: Vec<_> = outcome.results.iter().map(|r| r.status.clone()).collect();
    assert_eq!(statuses, vec![PushRefStatus::Ok]);

    assert_eq!(
        git(&bare, &["show-ref", "refs/heads/main"]).trim(),
        format!("{tip} refs/heads/main")
    );
    git(&bare, &["fsck", "--strict"]);
    assert_eq!(git(&work, &["for-each-ref", "refs/remotes/"]), "");
}
