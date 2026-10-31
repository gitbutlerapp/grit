//! Unit tests for hot-text linker detection (included source, not build.rs).

include!("../hot_text_linker_support.rs");

#[test]
fn default_rustflags_allow_hot_text() {
    // Without `-fuse-ld=bfd` in the environment, LLD (rustc default) is assumed.
    if std::env::var("CARGO_ENCODED_RUSTFLAGS").is_ok_and(|f| f.contains("fuse-ld=bfd"))
        || std::env::var("RUSTFLAGS").is_ok_and(|f| f.contains("fuse-ld=bfd"))
    {
        assert!(!linker_supports_hot_text_layout());
    } else {
        assert!(linker_supports_hot_text_layout());
    }
}
