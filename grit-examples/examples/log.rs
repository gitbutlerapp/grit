// API docs: https://docs.rs/grit-lib/latest/grit_lib/rev_list/index.html
use grit_lib::rev_list::{split_revision_token, RevListOptions};

fn main() {
    let (positive, negative) = split_revision_token("main..feature");
    let options = RevListOptions::default();

    println!("include {positive:?}, exclude {negative:?}");
    println!("reverse output? {}", options.reverse);
}
