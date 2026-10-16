//! `process_trailers` vs `git interpret-trailers --parse` on fixed messages.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::commit::trailers::interpret::{
    process_trailers, NewTrailerArg, ProcessTrailerOptions, TrailerIfExists, TrailerIfMissing,
    TrailerWhere,
};
use tempfile::TempDir;

fn git_interpret_trailers(repo: &Path, extra_args: &[&str], input: &str) -> String {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["interpret-trailers"])
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git interpret-trailers {:?} failed: {}",
        extra_args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn grit_parse(git_dir: Option<&Path>, input: &str, new_args: &[NewTrailerArg]) -> String {
    let opts = ProcessTrailerOptions {
        trim_empty: false,
        only_trailers: true,
        only_input: new_args.is_empty(),
        unfold: false,
        no_divider: true,
    };
    process_trailers(input, &opts, new_args, git_dir)
        .trim_end()
        .to_string()
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let status = Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .expect("git init");
    assert!(status.success());
    dir
}

#[test]
fn interpret_trailers_parse_matches_git() {
    let dir = init_repo();
    let git_dir = dir.path().join(".git");
    let cases = [
        "Subject\n\nHelped-by: Alice <a@example.com>\n",
        "Title\n\nSigned-off-by: Bob <b@example.com>\nReviewed-by: Carol\n",
        "No trailers yet\n",
    ];
    for msg in cases {
        let git_out = git_interpret_trailers(dir.path(), &["--parse"], msg);
        let grit_out = grit_parse(Some(git_dir.as_path()), msg, &[]);
        assert_eq!(
            grit_out, git_out,
            "trailer parse mismatch for message:\n{msg:?}"
        );
    }
}

#[test]
fn interpret_trailers_trailer_arg_matches_git() {
    let dir = init_repo();
    let git_dir = dir.path().join(".git");
    let input = "Subject\n\n";
    let git_out = git_interpret_trailers(dir.path(), &["--trailer", "Acked-by: Maintainer"], input);
    let opts = ProcessTrailerOptions {
        trim_empty: false,
        only_trailers: false,
        only_input: false,
        unfold: false,
        no_divider: false,
    };
    let args = [NewTrailerArg {
        text: "Acked-by: Maintainer".into(),
        where_: TrailerWhere::Default,
        if_exists: TrailerIfExists::Default,
        if_missing: TrailerIfMissing::Default,
    }];
    let grit_out = process_trailers(input, &opts, &args, Some(git_dir.as_path()));
    assert_eq!(grit_out.trim_end(), git_out.trim_end());
}
