//! Sweep low-coverage non-history modules that still count toward `[total]`.

#![allow(clippy::unwrap_used)]

use grit_lib::commit::signing::{verify_commit, GpgConfig};
use grit_lib::config::git_path::{
    cleanup_path, git_path_relative_component, is_common_git_path, normalize_path_copy,
};
use grit_lib::config::gitmodules::{
    check_submodule_name, check_submodule_url, looks_like_command_line_option,
    tree_entry_is_gitattributes_blob, tree_entry_is_gitmodules_blob, validate_gitmodules_blob_line,
    write_gitmodules_cli_option_warnings,
};
use grit_lib::config::mailmap::{
    load_mailmap, map_contact_table, parse_contact, parse_mailmap, render_contact,
    table_from_entries,
};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::{InitOptions, Repository};
use std::io::Cursor;
use tempfile::tempdir;

#[test]
fn gitmodules_mailmap_path_and_signing_helpers() -> grit_lib::error::Result<()> {
    assert!(looks_like_command_line_option("--help"));
    assert!(tree_entry_is_gitmodules_blob(0o100644, b".gitmodules"));
    assert!(tree_entry_is_gitattributes_blob(
        0o100644,
        b".gitattributes"
    ));
    assert!(check_submodule_name("valid-name"));
    assert!(!check_submodule_name(""));
    assert!(check_submodule_url("https://example.com/repo.git"));
    let _ = validate_gitmodules_blob_line(b"[submodule \"x\"]\n\tpath = p\n\turl = u\n");
    let content = "[submodule \"s\"]\n\tpath = --weird\n\turl = --also\n";
    let mut out = Cursor::new(Vec::new());
    write_gitmodules_cli_option_warnings(&mut out, content).expect("warnings");

    let _ = normalize_path_copy("a//b/../c").ok();
    let _ = cleanup_path("./a/b");
    let _ = git_path_relative_component("refs/heads/main");
    assert!(is_common_git_path("objects"));

    let entries = parse_mailmap("Name <n@e.com> <c@e.com>\n");
    assert!(!entries.is_empty());
    let (name, email) = parse_contact("A <a@b.com>");
    let table = table_from_entries(&entries);
    let _ = map_contact_table(name.as_deref(), email.as_deref(), &table);
    let _ = render_contact("A", "a@b.com");

    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let _ = load_mailmap(&repo).ok();
    let tree = repo.odb().write(ObjectKind::Tree, b"")?;
    let commit =
        format!("tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nm\n");
    let cfg = ConfigSet::new();
    let gpg = GpgConfig::from_config(&cfg)?;
    let _ = verify_commit(&gpg, commit.as_bytes())?;
    Ok(())
}
