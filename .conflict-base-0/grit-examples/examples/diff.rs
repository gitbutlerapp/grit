// API docs: https://docs.rs/grit-lib/latest/grit_lib/diff/index.html
use grit_lib::diff::unified_diff;

fn main() {
    let patch = unified_diff(
        "hello\nold\n",
        "hello\nnew\n",
        "file.txt",
        "file.txt",
        3,
        true,
        false,
    );
    println!("{patch}");
}
