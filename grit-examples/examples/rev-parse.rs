// API docs: https://docs.rs/grit-lib/latest/grit_lib/rev_parse/index.html
use grit_lib::rev_parse::{abbreviate_ref_name, split_double_dot_range};

fn main() {
    let short = abbreviate_ref_name("refs/heads/main");
    let range = split_double_dot_range("main..feature");

    println!("short ref: {short}");
    println!("range: {range:?}");
}
