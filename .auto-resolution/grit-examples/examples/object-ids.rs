// API docs: https://docs.rs/grit-lib/latest/grit_lib/objects/struct.ObjectId.html
use grit_lib::objects::ObjectId;

fn main() -> grit_lib::error::Result<()> {
    let id = ObjectId::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")?;

    println!("full id: {id}");
    println!("loose path: {}/{}", id.loose_prefix(), id.loose_suffix());
    Ok(())
}
