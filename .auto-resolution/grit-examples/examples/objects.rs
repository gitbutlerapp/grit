// API docs: https://docs.rs/grit-lib/latest/grit_lib/objects/index.html
use grit_lib::objects::{HashAlgo, Object, ObjectKind};

fn main() {
    let object = Object::new(ObjectKind::Blob, b"hello from grit\n".to_vec());
    let oid = HashAlgo::Sha1.hash_object(object.kind, &object.data);

    println!("{} {} bytes", oid, object.data.len());
}
