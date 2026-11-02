//! `release_unused_heap` returns freed glibc arena pages on Linux.

#![cfg(target_os = "linux")]

mod upload_pack_blob_memory_support;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use grit_lib::release_unused_heap;
use upload_pack_blob_memory_support::anon_rss_kib;

#[test]
fn release_unused_heap_drops_anon_rss_after_large_free() {
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }

    const CHUNK: usize = 32 * 1024;
    const CHUNKS: usize = 3072; // ~96 MiB

    let barrier = Arc::new(Barrier::new(2));
    let barrier_child = Arc::clone(&barrier);
    let handle = thread::spawn(move || {
        let mut chunks: Vec<Vec<u8>> = (0..CHUNKS).map(|i| vec![(i % 256) as u8; CHUNK]).collect();
        let pin = chunks.pop().expect("pin one chunk");
        chunks.clear();
        chunks.shrink_to_fit();
        let _pin = pin;
        barrier_child.wait();
        thread::sleep(Duration::from_secs(2));
    });

    barrier.wait();
    thread::sleep(Duration::from_millis(200));
    let before_trim = anon_rss_kib();
    assert!(
        before_trim > 64 * 1024,
        "expected elevated anon RSS before trim, got {before_trim} KiB"
    );

    assert!(release_unused_heap());
    thread::sleep(Duration::from_millis(100));
    let after_trim = anon_rss_kib();
    let drop_kib = before_trim.saturating_sub(after_trim);
    assert!(
        drop_kib > 32 * 1024,
        "expected anon RSS to drop by tens of MiB after trim, before={before_trim} after={after_trim} drop={drop_kib} KiB"
    );

    handle.join().expect("worker thread");
}
