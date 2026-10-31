//! Link `grit` with `hot-text.ld` on Linux (see `scripts/gen_hot_text_layout.py`).
//!
//! The fragment only groups already-present input sections into one output section placed
//! before `.text` (`INSERT BEFORE .text`), so the default layout is otherwise untouched. Both
//! LLD (the default Rust linker on x86_64 Linux) and GNU ld accept it. Set
//! `GRIT_HOT_TEXT_LAYOUT=0` to link without it, e.g. with a linker that lacks `INSERT`.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=hot-text.ld");
    println!("cargo:rerun-if-env-changed=GRIT_HOT_TEXT_LAYOUT");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("GRIT_HOT_TEXT_LAYOUT").as_deref() == Ok("0")
    {
        return;
    }
    let Some(dir) = env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    let script = PathBuf::from(dir).join("hot-text.ld");
    if script.is_file() {
        println!("cargo:rustc-link-arg-bin=grit=-Wl,-T,{}", script.display());
    }
}
