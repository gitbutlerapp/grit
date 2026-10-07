// API docs: https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html
use grit_lib::objects::ObjectKind;
use grit_lib::odb::Odb;

fn main() {
    let oid = Odb::hash_object_data(ObjectKind::Blob, b"content to hash\n");
    println!("{oid}");
}
