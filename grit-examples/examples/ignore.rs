// API docs: https://docs.rs/grit-lib/latest/grit_lib/ignore/index.html
use grit_lib::ignore::{parse_sparse_patterns_from_blob, path_matches_sparse_pattern_list};

fn main() {
    let patterns = parse_sparse_patterns_from_blob("/*\n/docs/\n!/target/\n");
    let included = path_matches_sparse_pattern_list("docs/index.html", &patterns).unwrap_or(false);

    println!("docs/index.html included? {included}");
}
