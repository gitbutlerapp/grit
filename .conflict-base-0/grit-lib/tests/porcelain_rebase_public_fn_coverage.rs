//! Execute public `porcelain::rebase` helpers for llvm-cov / public-fn coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::{CommitData, ObjectId};
use grit_lib::porcelain::rebase::{
    append_commented, append_nth_squash_message, append_skipped_squash_message,
    commit_subject_single_line, copy_section, first_line_len, fixup_replacement_message,
    format_autosquash_subject_for_match, message_body_after_subject,
    rebase_todo_command_for_display, rebase_todo_command_for_display_abbrev, skip_blank_lines,
    skip_fixupish_prefix, squash_comment_subject_prefix, strip_fixupish_chain,
    update_squash_message_for_fixup, RebaseTodoCmd,
};

fn empty_tree() -> ObjectId {
    ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree")
}

fn commit_with_message(message: &str) -> CommitData {
    CommitData {
        tree: empty_tree(),
        parents: Vec::new(),
        author: "Author <author@example.com>".to_owned(),
        committer: "Committer <committer@example.com>".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: message.to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    }
}

#[test]
fn rebase_todo_cmd_keywords() {
    assert_eq!(RebaseTodoCmd::Pick.as_str(), "pick");
    assert_eq!(RebaseTodoCmd::parse_word("f"), Some(RebaseTodoCmd::Fixup));
    assert!(RebaseTodoCmd::parse_word("exec").is_none());
}

#[test]
fn rebase_subject_and_fixup_helpers() {
    let msg = "subject line\n\nbody\n";
    assert_eq!(commit_subject_single_line(msg), "subject line");
    assert_eq!(format_autosquash_subject_for_match(msg), "subject line");
    assert_eq!(skip_fixupish_prefix("fixup! foo"), Some("foo"));
    assert_eq!(strip_fixupish_chain("fixup! squash! tail"), "tail");
    assert_eq!(message_body_after_subject(msg), "\nbody\n");
    assert_eq!(skip_blank_lines("\n\nrest"), "rest");
    assert_eq!(first_line_len("abc\ndef"), 3);
}

#[test]
fn rebase_todo_display_and_fixup_message() {
    let normal = commit_with_message("plain subject\n");
    assert_eq!(
        rebase_todo_command_for_display(RebaseTodoCmd::Pick, &normal),
        "pick"
    );
    assert_eq!(
        rebase_todo_command_for_display_abbrev(RebaseTodoCmd::Squash, &normal, true),
        "s"
    );
    let amend = commit_with_message("amend! tweak\n\nbody\n");
    assert_eq!(
        rebase_todo_command_for_display(RebaseTodoCmd::Fixup, &amend),
        "fixup -C"
    );
    assert_eq!(
        rebase_todo_command_for_display_abbrev(RebaseTodoCmd::Fixup, &amend, true),
        "f -C"
    );
    let _ = fixup_replacement_message("amend! x\n\ny\n");
    let _ = fixup_replacement_message("regular\n");
}

#[test]
fn rebase_squash_buffer_assembly() {
    let body = "squash! subject\n\nparagraph\n";
    let _ = squash_comment_subject_prefix(body, RebaseTodoCmd::Squash, false);
    let mut buf = String::from("seed\n\n");
    append_commented(&mut buf, "comment me");
    update_squash_message_for_fixup(&mut buf);
    copy_section(&mut buf, "section\n", true);
    append_skipped_squash_message(&mut buf, body, 2);
    append_nth_squash_message(&mut buf, body, RebaseTodoCmd::Fixup, true, 1);
    assert!(buf.contains('#'));
}
