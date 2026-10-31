//! `grit` enables grit-lib's `hot-text-layout` feature, so on x86_64 Linux GNU the library's
//! hot code path shares one `.text.grit_hot` output section ahead of `.text` instead of being
//! scattered across it (see `grit-lib/build.rs` and `scripts/gen_hot_text_layout.py`).

#![cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]

use std::error::Error;

/// Below this the fragment has gone stale: most of its patterns no longer match anything.
const MIN_HOT_BYTES: u64 = 256 * 1024;

struct Section {
    name: Vec<u8>,
    addr: u64,
    size: u64,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Section headers of a little-endian ELF64 file.
fn sections(elf: &[u8]) -> Option<Vec<Section>> {
    if elf.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let shoff = usize::try_from(u64_at(elf, 0x28)?).ok()?;
    let entsize = usize::from(u16_at(elf, 0x3a)?);
    let count = usize::from(u16_at(elf, 0x3c)?);
    let strndx = usize::from(u16_at(elf, 0x3e)?);
    let header = |i: usize| shoff + i * entsize;
    let strtab = usize::try_from(u64_at(elf, header(strndx) + 0x18)?).ok()?;
    (0..count)
        .map(|i| {
            let h = header(i);
            let name_at = strtab + usize::try_from(u32_at(elf, h)?).ok()?;
            let rest = elf.get(name_at..)?;
            let name = rest[..rest.iter().position(|&b| b == 0)?].to_vec();
            Some(Section {
                name,
                addr: u64_at(elf, h + 0x10)?,
                size: u64_at(elf, h + 0x20)?,
            })
        })
        .collect()
}

#[cfg(not(grit_hot_text_layout_disabled))]
#[test]
fn grit_binary_has_hot_text_section_ahead_of_text() -> Result<(), Box<dyn Error>> {
    if option_env!("GRIT_HOT_TEXT_LAYOUT") == Some("0") {
        return Ok(());
    }
    let binary = std::fs::read(env!("CARGO_BIN_EXE_grit"))?;
    let sections = sections(&binary).ok_or("grit is not a little-endian ELF64 file")?;
    let find = |name: &[u8]| sections.iter().find(|s| s.name == name);
    let hot = find(b".text.grit_hot").ok_or("grit was linked without grit-lib/hot-text.ld")?;
    let text = find(b".text").ok_or("grit has no .text section")?;
    assert!(
        hot.size >= MIN_HOT_BYTES,
        ".text.grit_hot holds only {} bytes; regenerate grit-lib/hot-text.ld",
        hot.size
    );
    assert!(
        hot.addr + hot.size <= text.addr,
        ".text.grit_hot is not ahead of .text"
    );
    Ok(())
}
