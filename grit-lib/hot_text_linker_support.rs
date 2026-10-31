// Shared by `grit-lib/build.rs` and `grit-cli/build.rs` (via `include!`).

/// Collect linker-related flags from the environment Cargo passes to build scripts.
pub fn linker_rustflags() -> Vec<String> {
    let mut args = Vec::new();
    if let Ok(encoded) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        args.extend(
            encoded
                .split('\x1f')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    if let Ok(flags) = std::env::var("RUSTFLAGS") {
        args.extend(flags.split_whitespace().map(str::to_string));
    }
    args
}

/// Whether the effective link uses LLD (or rustc's default LLD on this target).
///
/// CI sets `RUSTFLAGS=-C link-arg=-fuse-ld=bfd` to avoid rust-lld bus errors on huge test
/// binaries; GNU bfd then treats `libgrit_lib_hot_text.so` as a shared object and fails.
pub fn linker_supports_hot_text_layout() -> bool {
    let mut fuse_ld: Option<String> = None;
    for arg in linker_rustflags() {
        if let Some(ld) = arg.strip_prefix("-fuse-ld=") {
            fuse_ld = Some(ld.to_string());
            continue;
        }
        if let Some(rest) = arg.strip_prefix("link-arg=") {
            if let Some(ld) = rest.strip_prefix("-fuse-ld=") {
                fuse_ld = Some(ld.to_string());
            }
        }
    }
    matches!(fuse_ld.as_deref(), None | Some("lld"))
}
