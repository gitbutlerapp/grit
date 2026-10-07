// API docs: https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html
use grit_lib::index::Index;

fn main() {
    let index = Index::new();
    println!("empty index has {} entries", index.entries.len());
}
