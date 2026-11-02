//! Return freed heap pages to the operating system where supported.

/// Release memory the global allocator has freed but may still hold in arenas.
///
/// On Linux (glibc and musl), this calls `malloc_trim(0)`, which returns unused
/// arena pages to the kernel. Live allocations are untouched. On other targets
/// this is a no-op.
///
/// Call after large, bursty allocations (for example pack generation) when no
/// concurrent work on the same thread needs the allocator — `malloc_trim` can
/// briefly lock every arena.
#[must_use]
pub fn release_unused_heap() -> bool {
    release_unused_heap_impl()
}

#[cfg(target_os = "linux")]
fn release_unused_heap_impl() -> bool {
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    // SAFETY: `malloc_trim` is a standard libc extension on Linux; `pad` is 0 per man page.
    unsafe { malloc_trim(0) != 0 }
}

#[cfg(not(target_os = "linux"))]
fn release_unused_heap_impl() -> bool {
    false
}
