// API docs: https://docs.rs/grit-lib/latest/grit_lib/merge_file/index.html
use grit_lib::diff::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};

fn main() -> grit_lib::error::Result<()> {
    let result = merge(
        &MergeInput::new(b"base\n", b"ours\n", b"theirs\n")
            .with_label_ours("ours")
            .with_label_base("base")
            .with_label_theirs("theirs")
            .with_favor(MergeFavor::None)
            .with_style(ConflictStyle::Merge)
            .with_marker_size(7),
    )?;

    println!("{}", String::from_utf8_lossy(&result.content));
    Ok(())
}
