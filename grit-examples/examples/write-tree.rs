// API docs: https://docs.rs/grit-lib/latest/grit_lib/write_tree/index.html
use grit_lib::index::Index;
use grit_lib::odb::Odb;
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use std::path::Path;

fn main() -> grit_lib::error::Result<()> {
    let odb = Odb::new(Path::new(".git/objects"));
    let mut index = Index::new();
    let tree_id = write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default())?;

    println!("empty tree: {tree_id}");
    Ok(())
}
