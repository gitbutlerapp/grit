// API docs: https://docs.rs/grit-lib/latest/grit_lib/state/fn.resolve_head.html
use grit_lib::state::{resolve_head, HeadState};
use std::path::Path;

fn main() -> grit_lib::error::Result<()> {
    match resolve_head(Path::new(".git"))? {
        HeadState::Branch { refname, oid, .. } => println!("{refname} at {oid:?}"),
        HeadState::Detached { oid } => println!("detached at {oid}"),
        HeadState::Invalid => println!("HEAD is invalid"),
    }
    Ok(())
}
