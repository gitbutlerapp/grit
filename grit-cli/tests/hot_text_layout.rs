//! On Linux, `grit` links with `hot-text.ld`, so the functions a status run executes share one
//! `.text.grit_hot` output section instead of being scattered across `.text` (see
//! `scripts/gen_hot_text_layout.py`).

#![cfg(target_os = "linux")]

use std::error::Error;

#[test]
fn grit_binary_has_hot_text_section() -> Result<(), Box<dyn Error>> {
    if option_env!("GRIT_HOT_TEXT_LAYOUT") == Some("0") {
        return Ok(());
    }
    let binary = std::fs::read(env!("CARGO_BIN_EXE_grit"))?;
    assert!(
        binary.split(|&b| b == 0).any(|s| s == b".text.grit_hot"),
        "grit was linked without grit-cli/hot-text.ld"
    );
    Ok(())
}
