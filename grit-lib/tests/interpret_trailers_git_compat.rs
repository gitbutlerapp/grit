//! `process_trailers` vs `git interpret-trailers` on fixed messages, placement options and config.

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

/// Runs `git interpret-trailers` with `args` on `input` and returns stdout unmodified.
fn git_interpret_trailers_raw(repo: &Path, args: &[String], input: &str) -> String {
    let mut child = Command::new("git")
        .current_dir(repo)
        .arg("interpret-trailers")
        .args(args)
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
        "git interpret-trailers {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 output")
}

fn git_config(repo: &Path, key: &str, value: &str) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(["config", key, value])
        .status()
        .expect("git config");
    assert!(status.success(), "git config {key} {value}");
}

const WHERES: [(TrailerWhere, Option<&str>); 5] = [
    (TrailerWhere::Default, None),
    (TrailerWhere::End, Some("end")),
    (TrailerWhere::Start, Some("start")),
    (TrailerWhere::After, Some("after")),
    (TrailerWhere::Before, Some("before")),
];

const IF_EXISTS: [(TrailerIfExists, Option<&str>); 6] = [
    (TrailerIfExists::Default, None),
    (
        TrailerIfExists::AddIfDifferentNeighbor,
        Some("addIfDifferentNeighbor"),
    ),
    (TrailerIfExists::AddIfDifferent, Some("addIfDifferent")),
    (TrailerIfExists::Add, Some("add")),
    (TrailerIfExists::Replace, Some("replace")),
    (TrailerIfExists::DoNothing, Some("doNothing")),
];

const IF_MISSING: [(TrailerIfMissing, Option<&str>); 3] = [
    (TrailerIfMissing::Default, None),
    (TrailerIfMissing::Add, Some("add")),
    (TrailerIfMissing::DoNothing, Some("doNothing")),
];

const MATRIX_MESSAGES: &[&str] = &[
    "",
    "Subject\n",
    "Subject\n\nBody text.\n",
    "Subject\n\nBody.\n\nSigned-off-by: A <a@example.com>\nAcked-by: X\n",
    "Subject\n\nBody.\n\nAcked-by: X\nReviewed-by: Y\nAcked-by: W\n",
    "Subject\n\nBody.\n\nReviewed-by: Y\n  continued value\nEmpty-by:\n",
    "Subject\n\nBody.\n\nSigned-off-by: A <a@example.com>\n---\npatch notes\n",
    "Subject\n\nBody\n# comment line\n\nAcked-by: X\n# trailing comment\n",
    "Subject\n\nBody\n\nConflicts:\n\tfile.c\n",
    "Subject\n\nnot a trailer line\nAcked-by: X\n",
    "Subject\n\nfree text\nmore text\nmore\nSigned-off-by: A <a@example.com>\n",
    "Subject\n\nBody\n\nBug #42\nAcked-by: X\n",
    "Subject\n\nBody\n\n# ------------------------ >8 ------------------------\n# diff\n",
    "Subject\n\nAcked-by: X",
];

const MATRIX_ARGS: &[&[&str]] = &[
    &["Acked-by: Z"],
    &["acked-BY: x"],
    &["Signed-off-by: A <a@example.com>"],
    &["Reviewed-by: Y", "Acked-by=Z", "Empty-by:"],
];

/// Compares `process_trailers` with `git interpret-trailers` for one message and argument list.
fn assert_args_match_git(
    repo: &Path,
    git_dir: &Path,
    input: &str,
    texts: &[&str],
    where_: (TrailerWhere, Option<&str>),
    if_exists: (TrailerIfExists, Option<&str>),
    if_missing: (TrailerIfMissing, Option<&str>),
) {
    let mut git_args = Vec::new();
    for (flag, value) in [
        ("--where", where_.1),
        ("--if-exists", if_exists.1),
        ("--if-missing", if_missing.1),
    ] {
        if let Some(value) = value {
            git_args.push(format!("{flag}={value}"));
        }
    }
    let mut new_args = Vec::new();
    for text in texts {
        git_args.push(format!("--trailer={text}"));
        new_args.push(NewTrailerArg {
            text: (*text).to_string(),
            where_: where_.0,
            if_exists: if_exists.0,
            if_missing: if_missing.0,
        });
    }
    let expected = git_interpret_trailers_raw(repo, &git_args, input);
    let actual = process_trailers(
        input,
        &ProcessTrailerOptions::default(),
        &new_args,
        Some(git_dir),
    );
    assert_eq!(actual, expected, "input {input:?}, git args {git_args:?}");
}

/// Runs the placement matrix (every `where` x `ifExists`, plus each `ifMissing`) for all
/// matrix messages and argument lists in the repository at `repo`.
fn assert_placement_matrix_matches_git(repo: &Path) {
    let git_dir = repo.join(".git");
    for input in MATRIX_MESSAGES {
        for texts in MATRIX_ARGS {
            for where_ in WHERES {
                for if_exists in IF_EXISTS {
                    assert_args_match_git(
                        repo,
                        &git_dir,
                        input,
                        texts,
                        where_,
                        if_exists,
                        IF_MISSING[0],
                    );
                }
            }
            for if_missing in IF_MISSING {
                assert_args_match_git(
                    repo,
                    &git_dir,
                    input,
                    texts,
                    WHERES[0],
                    IF_EXISTS[0],
                    if_missing,
                );
            }
        }
    }
}

#[test]
fn trailer_placement_matrix_matches_git() {
    let dir = init_repo();
    assert_placement_matrix_matches_git(dir.path());
}

#[test]
fn trailer_placement_matrix_with_global_config_matches_git() {
    let dir = init_repo();
    git_config(dir.path(), "trailer.where", "start");
    git_config(dir.path(), "trailer.ifexists", "replace");
    git_config(dir.path(), "trailer.ifmissing", "doNothing");
    assert_placement_matrix_matches_git(dir.path());
}

#[test]
fn trailer_placement_matrix_with_alias_config_matches_git() {
    let dir = init_repo();
    git_config(dir.path(), "trailer.where", "before");
    git_config(dir.path(), "trailer.ack.key", "Acked-by");
    git_config(dir.path(), "trailer.ack.where", "after");
    git_config(dir.path(), "trailer.ack.ifexists", "addIfDifferent");
    git_config(dir.path(), "trailer.review.key", "Reviewed-by: ");
    git_config(dir.path(), "trailer.review.ifmissing", "doNothing");
    git_config(dir.path(), "trailer.separators", ":#");
    assert_placement_matrix_matches_git(dir.path());
    let git_dir = dir.path().join(".git");
    for input in MATRIX_MESSAGES {
        for texts in [&["ack: Q"][..], &["review: R", "Bug #7"], &["ACK"]] {
            assert_args_match_git(
                dir.path(),
                &git_dir,
                input,
                texts,
                WHERES[0],
                IF_EXISTS[0],
                IF_MISSING[0],
            );
        }
    }
}

#[test]
fn trailer_command_config_matches_git() {
    let dir = init_repo();
    git_config(dir.path(), "trailer.sign.key", "Signed-off-by: ");
    git_config(dir.path(), "trailer.sign.command", "echo cmd-$ARG");
    git_config(dir.path(), "trailer.fix.cmd", "echo fixed");
    let git_dir = dir.path().join(".git");
    for input in MATRIX_MESSAGES {
        for texts in [&[][..], &["sign: me"], &["fix: 12"]] {
            assert_args_match_git(
                dir.path(),
                &git_dir,
                input,
                texts,
                WHERES[0],
                IF_EXISTS[0],
                IF_MISSING[0],
            );
        }
    }
}

#[test]
fn trailer_output_options_match_git() {
    let dir = init_repo();
    let git_dir = dir.path().join(".git");
    for input in MATRIX_MESSAGES {
        for bits in 0u8..32 {
            let opts = ProcessTrailerOptions {
                trim_empty: bits & 1 != 0,
                only_trailers: bits & 2 != 0,
                only_input: bits & 4 != 0,
                unfold: bits & 8 != 0,
                no_divider: bits & 16 != 0,
            };
            let mut git_args: Vec<String> = [
                (opts.trim_empty, "--trim-empty"),
                (opts.only_trailers, "--only-trailers"),
                (opts.only_input, "--only-input"),
                (opts.unfold, "--unfold"),
                (opts.no_divider, "--no-divider"),
            ]
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, flag)| flag.to_string())
            .collect();
            let new_args = if opts.only_input {
                Vec::new()
            } else {
                git_args.push("--trailer=Acked-by: Z".to_string());
                vec![NewTrailerArg {
                    text: "Acked-by: Z".into(),
                    where_: TrailerWhere::Default,
                    if_exists: TrailerIfExists::Default,
                    if_missing: TrailerIfMissing::Default,
                }]
            };
            let expected = git_interpret_trailers_raw(dir.path(), &git_args, input);
            let actual = process_trailers(input, &opts, &new_args, Some(git_dir.as_path()));
            assert_eq!(actual, expected, "input {input:?}, git args {git_args:?}");
        }
    }
}
