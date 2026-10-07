// API docs: https://docs.rs/grit-lib/latest/grit_lib/refs/index.html
//
// Network fetch paths reject malformed advertised ref names via
// `grit_lib::refs::is_valid_fetch_advertised_ref` before refspec mapping.
use grit_lib::refs::{list_refs, read_head};
use std::path::Path;

fn main() -> grit_lib::error::Result<()> {
    let git_dir = Path::new(".git");
    println!("HEAD = {:?}", read_head(git_dir)?);

    for (name, oid) in list_refs(git_dir, "refs/heads")? {
        println!("{name} {oid}");
    }
    Ok(())
}
