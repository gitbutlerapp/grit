//! Propagate whether hot-text layout will actually be linked (LLD only) to integration tests.

include!("../grit-lib/hot_text_linker_support.rs");

fn main() {
    println!("cargo:rerun-if-changed=../grit-lib/hot_text_linker_support.rs");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    if std::env::var("GRIT_HOT_TEXT_LAYOUT").as_deref() == Ok("0")
        || std::env::var("TARGET").as_deref() != Ok("x86_64-unknown-linux-gnu")
        || !linker_supports_hot_text_layout()
    {
        println!("cargo:rustc-cfg=grit_hot_text_layout_disabled");
    }
}
