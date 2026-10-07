// API docs: https://docs.rs/grit-lib/latest/grit_lib/merge_file/index.html
use grit_lib::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};

fn main() -> grit_lib::error::Result<()> {
    let result = merge(&MergeInput {
        base: b"base\n",
        ours: b"ours\n",
        theirs: b"theirs\n",
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor: MergeFavor::None,
        style: ConflictStyle::Merge,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    })?;

    println!("{}", String::from_utf8_lossy(&result.content));
    Ok(())
}
