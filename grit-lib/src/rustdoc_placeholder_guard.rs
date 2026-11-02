//! Regression guard for public rustdoc on API clarity items (issue #2494).

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    const WATCHED: &[&str] = &[
        "src/structured_diff.rs",
        "src/command_runner.rs",
        "src/diagnostics.rs",
        "src/rerere.rs",
        "src/gitmodules.rs",
        "src/commit_graph_file.rs",
        "src/commit_graph_write.rs",
    ];

    #[test]
    fn watched_sources_have_no_generated_rustdoc_placeholders() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut hits = Vec::new();
        for rel in WATCHED {
            let path = manifest.join(rel);
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
            for (line_no, line) in text.lines().enumerate() {
                if line.contains("for this item.") || line.contains("Returned when ") {
                    hits.push(format!("{}:{}: {line}", rel, line_no + 1));
                }
            }
        }
        assert!(hits.is_empty(), "placeholder rustdoc:\n{}", hits.join("\n"));
    }
}
