//! Integration coverage for [`grit_lib::hash`] parallel batch helpers.

use std::num::NonZeroUsize;

use grit_lib::hash::hash_object;
use grit_lib::hash::{
    hash_objects_parallel, par_hash_with, parallel_hash_worthwhile, try_par_hash_with,
    ParallelHashError, Parallelism, PAR_HASH_MIN_ITEMS, PAR_HASH_MIN_TOTAL_BYTES,
};
use grit_lib::objects::{HashAlgo, ObjectKind};

#[test]
fn hash_objects_parallel_public_api_matches_serial() {
    let data = b"parallel batch hashing integration test payload";
    let items = [(ObjectKind::Blob, data.as_slice()); 64];
    let serial: Vec<_> = items
        .iter()
        .map(|(k, d)| hash_object(HashAlgo::Sha1, *k, d))
        .collect();
    let parallel = hash_objects_parallel(
        HashAlgo::Sha1,
        &items,
        NonZeroUsize::new(4).expect("threads"),
    );
    assert_eq!(parallel, serial);
}

#[test]
fn try_par_hash_with_returns_typed_task_error() {
    let items = [1_i32, 2, 3];
    let err = try_par_hash_with(&items, NonZeroUsize::new(2).expect("threads"), 3, |&x| {
        if x == 2 {
            Err(42_u8)
        } else {
            Ok(x)
        }
    })
    .unwrap_err();
    assert_eq!(err, ParallelHashError::Task(42));
}

#[test]
fn parallelism_and_threshold_constants_are_documented() {
    let p = Parallelism::resolve(Some(2));
    assert!(p.threads().get() >= 1);
    assert!(!parallel_hash_worthwhile(
        PAR_HASH_MIN_ITEMS - 1,
        PAR_HASH_MIN_TOTAL_BYTES,
        8
    ));
    assert!(parallel_hash_worthwhile(
        PAR_HASH_MIN_ITEMS,
        PAR_HASH_MIN_TOTAL_BYTES,
        8
    ));
}

#[test]
fn par_hash_with_lazy_file_style_closure() {
    let paths = ["a", "b", "c", "d"];
    let got = par_hash_with(&paths, NonZeroUsize::new(2).expect("threads"), 0, |s| {
        s.len()
    });
    assert_eq!(got, vec![1, 1, 1, 1]);
}
