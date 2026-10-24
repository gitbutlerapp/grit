//! Attach a note to a commit and list notes with [`RepositoryNotes`](grit_lib::repo_handles::RepositoryNotes).
//!
//! Source for the library guide "Notes" page (included in the docs site).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::notes::NotesRef;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{NoteAddOutcome, NotesWriteOptions};
use grit_lib::state::resolve_head;
use std::path::{Path, PathBuf};

const IDENT: &str = "Library Guide <guide@grit-scm.com> 1735689600 +0000";

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    let mut opts = RepositoryOptions::empty();
    if let Some(wt) = work_tree {
        opts = opts.with_work_tree(wt);
    }
    Repository::open(&git_dir, &opts)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;

    let head = resolve_head(repo.git_dir())?;
    let commit: ObjectId = head
        .oid()
        .cloned()
        .ok_or_else(|| grit_lib::error::Error::Message("HEAD has no commit".into()))?;

    let notes_ref = NotesRef::commits();
    let outcome = repo.notes().add(
        &notes_ref,
        &commit,
        b"library guide note\n",
        &NotesWriteOptions {
            message: "add note".into(),
            committer: IDENT.parse()?,
            author: None,
        },
        false,
    )?;
    println!("added={}", outcome == NoteAddOutcome::Added);

    let body = repo.notes().read(&notes_ref, &commit)?.unwrap_or_default();
    let text = String::from_utf8_lossy(&body);
    println!("note={text}");

    for note in repo.notes().list_with_bodies(&notes_ref, None)?.items {
        println!(
            "{} {}",
            note.object.to_hex(),
            String::from_utf8_lossy(&note.body).trim_end()
        );
    }

    Ok(())
}
