// API docs: https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html
use grit_lib::objects::{HashAlgo, ObjectKind};

fn main() {
    let oid = HashAlgo::Sha1.hash_object(ObjectKind::Blob, b"content to hash\n");
    println!("{oid}");
}
