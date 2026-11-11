// API docs: https://docs.rs/grit-lib/latest/grit_lib/objects/struct.CommitData.html
use grit_lib::objects::{serialize_commit, CommitData, ObjectId};

fn main() -> grit_lib::error::Result<()> {
    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904")?;
    let commit = CommitData::new(
        tree,
        Vec::new(),
        "A U Thor <author@example.com> 0 +0000",
        "C O Mitter <committer@example.com> 0 +0000",
        "initial commit\n",
    );

    let bytes = serialize_commit(&commit);
    println!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}
