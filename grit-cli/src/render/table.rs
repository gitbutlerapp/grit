//! Markdown table cell escaping.

/// Escape cell text so it stays on one Markdown table row.
#[must_use]
pub fn escape_table_cell(s: &str) -> String {
    let trimmed = s.trim_end_matches(['\n', '\r']);
    trimmed
        .replace('|', "\\|")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::escape_table_cell;

    #[test]
    fn escape_table_cell_trims_trailing_line_terminators() {
        assert_eq!(escape_table_cell("one\r\n"), "one");
        assert_eq!(escape_table_cell("two\n"), "two");
    }

    #[test]
    fn escape_table_cell_replaces_internal_newlines_and_pipes() {
        assert_eq!(escape_table_cell("a|b\nc"), "a\\|b\\nc");
    }
}
