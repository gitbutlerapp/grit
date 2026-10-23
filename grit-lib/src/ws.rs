//! ASCII whitespace helpers shared by the diff engine (`-w`, `-b`, blank-line hunks).

/// Git xdiff / `-w` / `-b` ASCII whitespace (not Unicode general whitespace).
#[must_use]
pub fn is_space_git(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Returns true if the line is empty or only ASCII whitespace.
#[must_use]
pub fn ws_blank_line(line: &str) -> bool {
    line.bytes().all(is_space_git)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_space_git_is_ascii_only() {
        for c in [b' ', b'\t', b'\n', b'\r', 0x0b, 0x0c] {
            assert!(is_space_git(c));
        }
        assert!(!is_space_git(b'a'));
        assert!(!is_space_git(0xa0));
    }

    #[test]
    fn blank_line_detection() {
        assert!(ws_blank_line(""));
        assert!(ws_blank_line(" \t\r\n"));
        assert!(!ws_blank_line("  x\n"));
        assert!(!ws_blank_line("\u{a0}\n"));
    }
}
