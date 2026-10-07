// API docs: https://docs.rs/grit-lib/latest/grit_lib/objects/index.html
use grit_lib::objects::{Object, ObjectKind};
use grit_lib::odb::Odb;

fn main() {
    let object = Object::new(ObjectKind::Blob, b"hello from grit\n".to_vec());
    let oid = Odb::hash_object_data(object.kind, &object.data);

    println!("{} {} bytes", oid, object.data.len());
}
