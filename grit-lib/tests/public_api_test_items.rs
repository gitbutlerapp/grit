//! Ratchet: public API paths must not look like test/bench/fixture hooks unless allowlisted.

use std::collections::BTreeSet;
const SNAPSHOT: &str = include_str!("../public-api.txt");
const ALLOWLIST: &str = include_str!("../public-api-test-items.allow");

const FORBIDDEN: &[&str] = &[
    "test", "tests", "probe", "probes", "fixture", "fixtures", "bench", "benches",
];

/// Split a Rust path segment on `_` and CamelCase boundaries into lowercase words.
fn segment_words(segment: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for chunk in segment.split('_') {
        if chunk.is_empty() {
            continue;
        }
        let mut word = String::new();
        let mut prev_lower_or_digit = false;
        for (i, ch) in chunk.char_indices() {
            if ch.is_uppercase() {
                if i > 0 && prev_lower_or_digit {
                    parts.push(word.to_ascii_lowercase());
                    word.clear();
                } else if !word.is_empty()
                    && word.chars().all(|c| c.is_uppercase())
                    && chunk[i + ch.len_utf8()..]
                        .chars()
                        .next()
                        .is_some_and(|next| next.is_lowercase())
                {
                    parts.push(word.to_ascii_lowercase());
                    word.clear();
                }
                prev_lower_or_digit = false;
            } else {
                prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            }
            word.push(ch);
        }
        if !word.is_empty() {
            parts.push(word.to_ascii_lowercase());
        }
    }
    parts
}

fn path_violates(path: &str) -> bool {
    for segment in path.split("::") {
        for word in segment_words(segment) {
            if FORBIDDEN.contains(&word.as_str()) {
                return true;
            }
        }
    }
    false
}

fn normalize_grit_lib_path(raw: &str) -> String {
    let mut path = raw.to_string();
    if let Some(paren) = path.find('(') {
        path.truncate(paren);
    } else if let Some(type_sep) = path.rfind(": ") {
        path.truncate(type_sep);
    }
    path.trim_end_matches(':').trim().to_string()
}

fn item_paths_from_snapshot_line(line: &str) -> Vec<String> {
    let line = line.trim();
    let mut paths = Vec::new();
    let mut search = line;
    while let Some(idx) = search.find("grit_lib::") {
        let tail = &search[idx..];
        let end = tail
            .find(|c: char| {
                c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == '-' || c == '>'
            })
            .unwrap_or(tail.len());
        paths.push(normalize_grit_lib_path(&tail[..end]));
        search = &search[idx + "grit_lib::".len()..];
    }
    paths
}

fn offending_paths_from_snapshot(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        for path in item_paths_from_snapshot_line(line) {
            if path_violates(&path) {
                out.insert(path);
            }
        }
    }
    out
}

fn parse_allowlist(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn appears_in_snapshot(path: &str) -> bool {
    offending_paths_from_snapshot(SNAPSHOT).contains(path)
}

#[test]
fn public_api_test_items_ratchet() {
    let snapshot_offenders = offending_paths_from_snapshot(SNAPSHOT);
    let allow = parse_allowlist(ALLOWLIST);

    let missing: Vec<_> = snapshot_offenders.difference(&allow).collect();
    assert!(
        missing.is_empty(),
        "public API test-like paths missing from public-api-test-items.allow (remove hooks or \
         shrink the allowlist):\n{}",
        missing
            .iter()
            .map(|p| format!("  {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let stale: Vec<_> = allow
        .iter()
        .filter(|path| appears_in_snapshot(path) && !snapshot_offenders.contains(*path))
        .collect();
    assert!(
        stale.is_empty(),
        "stale allowlist entries for paths still listed in public-api.txt (drop the hook or \
         remove the allowlist line):\n{}",
        stale
            .iter()
            .map(|p| format!("  {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn word_splitter_unit_cases() {
    assert!(!path_violates("grit_lib::notes::NotesTree"));
    assert!(!path_violates("grit_lib::objects::WriteStats"));
    assert!(!path_violates("grit_lib::objects::CompositeStore"));
    assert!(!path_violates("grit_lib::refs::ReflogRewriteStale"));

    assert!(path_violates("grit_lib::bench_fixture"));
    assert!(path_violates(
        "grit_lib::remote::HttpClientFactory::create_for_probe"
    ));

    assert_eq!(
        segment_words("NotesTree"),
        vec!["notes".to_string(), "tree".to_string()]
    );
    assert_eq!(
        segment_words("WriteStats"),
        vec!["write".to_string(), "stats".to_string()]
    );
    assert_eq!(
        segment_words("ReflogRewriteStale"),
        vec![
            "reflog".to_string(),
            "rewrite".to_string(),
            "stale".to_string()
        ]
    );
}

#[test]
fn snapshot_line_parser_smoke() {
    let line = "pub fn grit_lib::objects::bench_fixture::daily_special::DailySpecialShape::expected_ref_count(self, bool) -> usize";
    let path = item_paths_from_snapshot_line(line)
        .into_iter()
        .find(|p| p.contains("expected_ref_count"))
        .expect("path");
    assert_eq!(
        path,
        "grit_lib::objects::bench_fixture::daily_special::DailySpecialShape::expected_ref_count"
    );
    assert!(path_violates(&path));
}

#[test]
fn allowlist_file_sorted() {
    let lines: Vec<_> = ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(
        lines, sorted,
        "public-api-test-items.allow must stay sorted"
    );
}
