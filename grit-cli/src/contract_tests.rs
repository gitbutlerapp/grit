//! Contract tests for CLI docs, schemas, and argv normalizers.

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use serde_json::Value;

    /// Options every command accepts; they're documented once on the docs
    /// overview page rather than on each command's page.
    const GLOBAL_OPTIONS: &[&str] = &[
        "help",
        "version",
        "json",
        "markdown",
        "filter",
        "threads",
        "no-optional-locks",
    ];

    /// Required `##` sections on every command page, in order. `Markdown output`
    /// may appear after `JSON output` when the command supports `--markdown`.
    const TEMPLATE_HEADINGS: &[&str] = &[
        "Synopsis",
        "Description",
        "Options",
        "Examples",
        "JSON output",
        "Exit status",
        "See also",
    ];

    /// Plumbing commands with no `--markdown` section on their doc page.
    const NO_MARKDOWN_SECTION_COMMANDS: &[&str] = &["manager", "upload-pack", "receive-pack"];

    /// Plumbing commands whose stdout is a wire protocol or credential stream,
    /// not a JSON outcome object.
    const NO_JSON_COMMANDS: &[&str] = &["completions", "manager", "upload-pack", "receive-pack"];

    /// Subcommands excluded from three-way output snapshot coverage (plumbing + shell completions).
    const OUTPUT_SNAPSHOT_EXEMPT: &[&str] = &[
        "help",
        "completions",
        "manager",
        "upload-pack",
        "receive-pack",
    ];

    /// In-process render snapshots that satisfy coverage for a top-level command.
    const RENDER_SNAPSHOT_FOR_COMMAND: &[(&str, &str)] = &[
        ("auth", "auth_render"),
        ("checkin", "checkin_render"),
        ("checkout", "checkout_render"),
        ("integrate", "integrate_render"),
        ("share", "share_render"),
        ("slice", "slice_render"),
        ("sync", "sync_render"),
        ("serve", "serve_render"),
        ("update", "update_render"),
    ];

    fn commands_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/docs/commands")
    }

    fn split_front_matter(text: &str) -> (&str, &str) {
        let Some(rest) = text.strip_prefix("---\n") else {
            return ("", text);
        };
        let Some((_, body)) = rest.split_once("\n---\n") else {
            return ("", text);
        };
        ("", body)
    }

    fn h2_headings(body: &str) -> Vec<String> {
        let mut headings = Vec::new();
        let mut in_fenced_block = false;
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_fenced_block = !in_fenced_block;
                continue;
            }
            if in_fenced_block {
                continue;
            }
            if let Some(heading) = line.strip_prefix("## ") {
                headings.push(heading.trim().to_owned());
            }
        }
        headings
    }

    fn section_body(body: &str, heading: &str) -> Option<String> {
        let marker = format!("## {heading}");
        let start = body.find(&marker)? + marker.len();
        let rest = &body[start..];
        let mut in_fenced = false;
        let mut end = rest.len();
        for (i, line) in rest.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_fenced = !in_fenced;
                continue;
            }
            if !in_fenced && i > 0 && line.starts_with("## ") {
                end = rest.lines().take(i).map(|l| l.len() + 1).sum();
                break;
            }
        }
        Some(rest[..end].trim().to_owned())
    }

    fn fenced_json_blocks(section: &str) -> Vec<(usize, String)> {
        const OPEN: &str = "```json";
        let mut blocks = Vec::new();
        let mut rest = section;
        let mut base = 0usize;
        while let Some(start) = rest.find(OPEN) {
            let abs_start = base + start + OPEN.len();
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find("```") else {
                break;
            };
            blocks.push((abs_start, after[..end].trim().to_owned()));
            let consumed = start + OPEN.len() + end + 3;
            base += consumed;
            rest = &rest[consumed..];
        }
        blocks
    }

    fn json_section_is_none(section: &str) -> bool {
        let lower = section.to_ascii_lowercase();
        lower.contains("none.")
            || lower.contains("no json")
            || lower.contains("has no effect")
            || lower.contains("always speaks")
    }

    fn command_supports_markdown(command: &clap::Command) -> bool {
        command
            .get_arguments()
            .any(|arg| arg.get_id() == "markdown" || arg.get_long() == Some("markdown"))
    }

    fn with_cli_root<R>(run: impl FnOnce(clap::Command) -> R + Send + 'static) -> R
    where
        R: Send + 'static,
    {
        const STACK: usize = 16 * 1024 * 1024;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || run(crate::cli_command::build_cli_command()))
            .expect("spawn cli root builder")
            .join()
            .expect("join cli root builder")
    }

    fn cli_supports_markdown() -> bool {
        with_cli_root(|root| {
            root.get_arguments()
                .any(|arg| arg.get_id() == "markdown" || arg.get_long() == Some("markdown"))
        })
    }

    fn top_level_keys(value: &Value) -> HashSet<String> {
        match value {
            Value::Object(map) => map.keys().cloned().collect(),
            _ => HashSet::new(),
        }
    }

    /// Keys named in the first column of a markdown field table inside the JSON section.
    fn field_table_top_level_keys(section: &str) -> HashSet<String> {
        let mut keys = HashSet::new();
        let mut past_header = false;
        for line in section.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('|') {
                continue;
            }
            if trimmed.contains("---") {
                past_header = true;
                continue;
            }
            if !past_header {
                continue;
            }
            let Some(first_cell) = trimmed.split('|').nth(1) else {
                continue;
            };
            let cell = first_cell.trim().trim_matches('`');
            let top = cell.split(['.', '[']).next().unwrap_or(cell).trim();
            if top.is_empty() || top.eq_ignore_ascii_case("field") {
                continue;
            }
            keys.insert(top.to_owned());
        }
        keys
    }

    fn json_section_has_field_table(section: &str) -> bool {
        let has_header = section
            .lines()
            .any(|line| line.contains('|') && line.to_lowercase().contains("field"));
        let has_separator = section
            .lines()
            .any(|line| line.trim().starts_with('|') && line.contains("---"));
        has_header && has_separator && !field_table_top_level_keys(section).is_empty()
    }

    fn page_mentions_markdown(body: &str) -> bool {
        body.contains("## Markdown output") || body.contains("--markdown")
    }

    /// Validate `##` heading order, allowing `Markdown output` only between JSON output and Exit status.
    fn validate_page_headings(name: &str, headings: &[String]) -> Vec<String> {
        const EXIT_STATUS_IDX: usize = 5;
        let mut problems = Vec::new();
        let mut template_idx = 0usize;
        let mut heading_idx = 0usize;
        while heading_idx < headings.len() {
            let heading = &headings[heading_idx];
            if template_idx < TEMPLATE_HEADINGS.len() && heading == TEMPLATE_HEADINGS[template_idx]
            {
                template_idx += 1;
                heading_idx += 1;
                continue;
            }
            if heading == "Markdown output" {
                if template_idx == EXIT_STATUS_IDX {
                    heading_idx += 1;
                    continue;
                }
                if template_idx > EXIT_STATUS_IDX {
                    problems.push(format!("{name}.md: unexpected ## Markdown output"));
                } else {
                    problems.push(format!(
                        "{name}.md: ## Markdown output must appear only after ## JSON output and before ## Exit status"
                    ));
                }
                heading_idx += 1;
                continue;
            }
            if template_idx < TEMPLATE_HEADINGS.len() {
                problems.push(format!(
                    "{name}.md: expected ## {} but found ## {heading}",
                    TEMPLATE_HEADINGS[template_idx]
                ));
            } else {
                problems.push(format!("{name}.md: unexpected ## {heading}"));
            }
            heading_idx += 1;
            template_idx = TEMPLATE_HEADINGS.len();
        }
        if template_idx < TEMPLATE_HEADINGS.len() {
            problems.push(format!(
                "{name}.md: missing ## {}",
                TEMPLATE_HEADINGS[template_idx]
            ));
        }
        problems
    }

    fn markdown_section_has_fenced_example(section: &str) -> bool {
        const OPEN: &str = "```";
        let mut rest = section;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let lang_end = after.find('\n').unwrap_or(after.len());
            let lang = after[..lang_end].trim();
            if !lang.is_empty()
                && lang != "json"
                && (lang == "markdown"
                    || lang == "text"
                    || lang == "diff"
                    || lang == "console"
                    || lang.starts_with("diff"))
            {
                let body_start = lang_end + 1;
                if body_start > after.len() {
                    break;
                }
                let tail = &after[body_start..];
                let close = tail
                    .find("\n```")
                    .map(|i| body_start + i)
                    .unwrap_or(after.len());
                if close > body_start && !after[body_start..close].trim().is_empty() {
                    return true;
                }
                if close < after.len() {
                    rest = &after[close + 4..];
                    continue;
                }
                break;
            }
            let Some(close) = after.find("```") else {
                break;
            };
            rest = &after[close + 3..];
        }
        false
    }

    fn validate_json_example_against_schema(
        name: &str,
        block_index: usize,
        schema_key: &str,
        instance: &Value,
    ) -> Option<String> {
        use crate::json_schema::load_committed_schema;
        let schema_value = load_committed_schema(schema_key);
        let compiled = match jsonschema::validator_for(&schema_value) {
            Ok(c) => c,
            Err(e) => {
                return Some(format!(
                    "{name}.md: JSON example {block_index}: compile schema `{schema_key}`: {e}"
                ));
            }
        };
        if compiled.is_valid(instance) {
            return None;
        }
        let msg = compiled
            .iter_errors(instance)
            .next()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "invalid".into());
        Some(format!(
            "{name}.md: JSON example {block_index} does not match schema `{schema_key}`: {msg}"
        ))
    }

    /// Every command page uses the documented section order.
    #[test]
    fn command_pages_follow_template() {
        with_cli_root(|root| {
            let dir = commands_dir();
            let mut problems = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" {
                    continue;
                }
                let path = dir.join(format!("{name}.md"));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    problems.push(format!("missing page {}", path.display()));
                    continue;
                };
                let (_, body) = split_front_matter(&text);
                problems.extend(validate_page_headings(name, &h2_headings(body)));
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    #[test]
    fn command_pages_follow_template_rejects_markdown_after_exit_status() {
        let headings = vec![
            "Synopsis".into(),
            "Description".into(),
            "Options".into(),
            "Examples".into(),
            "JSON output".into(),
            "Exit status".into(),
            "See also".into(),
            "Markdown output".into(),
        ];
        let problems = validate_page_headings("commit", &headings);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unexpected ## Markdown output")),
            "{problems:?}"
        );
    }

    #[test]
    fn command_pages_follow_template_allows_markdown_before_exit_status() {
        let headings = vec![
            "Synopsis".into(),
            "Description".into(),
            "Options".into(),
            "Examples".into(),
            "JSON output".into(),
            "Markdown output".into(),
            "Exit status".into(),
            "See also".into(),
        ];
        assert!(validate_page_headings("commit", &headings).is_empty());
    }

    #[test]
    fn h2_headings_ignores_headings_inside_fenced_blocks() {
        let body = r##"## Synopsis

## Markdown output

Example:

```markdown
## Garbage collection

### Tasks
```

## See also
"##;
        assert_eq!(
            h2_headings(body),
            vec![
                "Synopsis".to_owned(),
                "Markdown output".to_owned(),
                "See also".to_owned(),
            ]
        );
    }

    /// JSON examples in the JSON output section must parse, and document top-level keys.
    #[test]
    fn json_examples_parse() {
        with_cli_root(|root| {
            let dir = commands_dir();
            let mut problems = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" {
                    continue;
                }
                let path = dir.join(format!("{name}.md"));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (_, body) = split_front_matter(&text);
                let Some(section) = section_body(body, "JSON output") else {
                    problems.push(format!("{name}.md: no JSON output section"));
                    continue;
                };
                let blocks = fenced_json_blocks(&section);
                if NO_JSON_COMMANDS.contains(&name) {
                    if !blocks.is_empty() {
                        problems.push(format!(
                            "{name}.md: plumbing command must not include ```json examples"
                        ));
                    }
                    if !json_section_is_none(&section) {
                        problems.push(format!(
                            "{name}.md: JSON output section must explain that there is no JSON"
                        ));
                    }
                    continue;
                }
                if blocks.is_empty() {
                    problems.push(format!(
                        "{name}.md: JSON output section needs a ```json example"
                    ));
                    continue;
                }
                if !json_section_has_field_table(&section) {
                    problems.push(format!(
                        "{name}.md: JSON output section needs a field table (| Field | … |)"
                    ));
                    continue;
                }
                let table_keys = field_table_top_level_keys(&section);
                for (i, (_, block)) in blocks.iter().enumerate() {
                    let parsed: Value = serde_json::from_str(block).unwrap_or_else(|e| {
                        problems.push(format!("{name}.md: JSON example {i} does not parse: {e}"));
                        Value::Null
                    });
                    if parsed.is_null() && block != "null" {
                        continue;
                    }
                    for key in top_level_keys(&parsed) {
                        if !table_keys.contains(&key) {
                            problems.push(format!(
                            "{name}.md: JSON example documents `{key}` but the field table does not"
                        ));
                        }
                    }
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    /// Fenced JSON in the JSON output section must validate against the mapped v1/v2 schema.
    #[test]
    fn json_examples_validate_against_schemas() {
        use crate::json_schema::schema_key_for_doc_example;
        with_cli_root(|root| {
            let dir = commands_dir();
            let mut problems = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" || NO_JSON_COMMANDS.contains(&name) {
                    continue;
                }
                let path = dir.join(format!("{name}.md"));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    problems.push(format!("missing page {}", path.display()));
                    continue;
                };
                let (_, body) = split_front_matter(&text);
                let Some(section) = section_body(body, "JSON output") else {
                    problems.push(format!("{name}.md: no JSON output section"));
                    continue;
                };
                for (i, (idx, block)) in fenced_json_blocks(&section).iter().enumerate() {
                    let before = &section[..*idx];
                    let parsed: Value = match serde_json::from_str(block) {
                        Ok(v) => v,
                        Err(e) => {
                            problems
                                .push(format!("{name}.md: JSON example {i} does not parse: {e}"));
                            continue;
                        }
                    };
                    let Some(schema_key) = schema_key_for_doc_example(name, before, &parsed) else {
                        problems.push(format!("{name}.md: JSON example {i} has no schema mapping"));
                        continue;
                    };
                    if let Some(problem) =
                        validate_json_example_against_schema(name, i, &schema_key, &parsed)
                    {
                        problems.push(problem);
                    }
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    /// Markdown output sections must include a fenced real `--markdown` example (not prose alone).
    #[test]
    fn markdown_section_has_real_example() {
        with_cli_root(|root| {
            let dir = commands_dir();
            let mut problems = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" {
                    continue;
                }
                if NO_MARKDOWN_SECTION_COMMANDS.contains(&name) {
                    let path = dir.join(format!("{name}.md"));
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    let (_, body) = split_front_matter(&text);
                    if section_body(body, "Markdown output").is_some() {
                        problems.push(format!(
                            "{name}.md: plumbing command must not have ## Markdown output"
                        ));
                    }
                    continue;
                }
                if !page_requires_markdown(command) {
                    continue;
                }
                let path = dir.join(format!("{name}.md"));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    problems.push(format!("missing page {}", path.display()));
                    continue;
                };
                let (_, body) = split_front_matter(&text);
                let Some(section) = section_body(body, "Markdown output") else {
                    problems.push(format!("{name}.md: missing ## Markdown output section"));
                    continue;
                };
                if !markdown_section_has_fenced_example(&section) {
                    problems.push(format!(
                        "{name}.md: Markdown output needs a fenced example (text, markdown, diff, or console)"
                    ));
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    #[test]
    fn markdown_section_has_fenced_example_clone_page() {
        let text = std::fs::read_to_string(commands_dir().join("clone.md")).expect("clone.md");
        let (_, body) = split_front_matter(&text);
        let section = section_body(body, "Markdown output").expect("markdown section");
        let first_fence = section.find("```").unwrap_or(usize::MAX);
        assert!(
            markdown_section_has_fenced_example(&section),
            "clone markdown section should include a non-empty fenced example (first ``` at {first_fence}, len {})",
            section.len()
        );
    }

    #[test]
    fn markdown_section_has_real_example_only_plumbing_lacks_section() {
        with_cli_root(|root| {
            let dir = commands_dir();
            let mut without_section = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" {
                    continue;
                }
                let path = dir.join(format!("{name}.md"));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (_, body) = split_front_matter(&text);
                if section_body(body, "Markdown output").is_none() {
                    without_section.push(name.to_owned());
                }
            }
            without_section.sort();
            let mut expected: Vec<_> = NO_MARKDOWN_SECTION_COMMANDS
                .iter()
                .map(|s| s.to_string())
                .collect();
            expected.sort();
            assert_eq!(
                without_section, expected,
                "only manager, upload-pack, and receive-pack omit ## Markdown output"
            );
        });
    }

    #[test]
    fn json_examples_parse_requires_field_table_not_prose() {
        let section = r#"Pass `--json` for output.

The keys are `initialized`, `path`, `bare`, `branch`, `object_format`, and optionally `git_dir`.

```json
{"initialized": true, "path": "/tmp/x", "bare": false, "branch": "main", "object_format": "sha1"}
```
"#;
        assert!(!json_section_has_field_table(section));
        let keys = field_table_top_level_keys(section);
        assert!(keys.is_empty());
    }

    /// `--markdown` is documented only when clap exposes a `markdown` flag.
    #[test]
    fn markdown_output_only_when_supported() {
        with_cli_root(|root| {
            let dir = commands_dir();
            let global_markdown = cli_supports_markdown();
            let mut problems = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" {
                    continue;
                }
                let path = dir.join(format!("{name}.md"));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (_, body) = split_front_matter(&text);
                let mentions_markdown = page_mentions_markdown(body);
                let supported = global_markdown || command_supports_markdown(command);
                if mentions_markdown && !supported {
                    problems.push(format!(
                        "{name}.md documents --markdown but the command has no markdown flag"
                    ));
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    #[test]
    fn markdown_output_only_when_supported_detects_unbackticked_flag() {
        let body = "## JSON output\n\nPass --markdown for agent output.\n";
        assert!(page_mentions_markdown(body));
    }

    /// Each library area from the project direction document must have a CLI entry point.
    #[test]
    fn every_library_area_has_a_command() {
        const LIBRARY_AREAS: &[(&str, &[&str])] = &[
            ("object database", &["cat"]),
            ("network transport", &["fetch"]),
            ("refs", &["branch"]),
            ("index", &["add"]),
            ("stash", &["stash"]),
            ("reflog", &["reflog"]),
            ("diffing", &["diff"]),
            ("revwalking", &["revs"]),
            ("revparsing", &["resolve"]),
            ("config", &["config"]),
            ("ignore rules", &["ignore", "check"]),
            ("hooks", &["hooks"]),
            ("patch-ids", &["patch-id"]),
            ("notes", &["notes"]),
            ("bundles", &["bundle", "unbundle"]),
            ("fsck", &["fsck"]),
            ("worktrees", &["worktree", "lock"]),
        ];

        fn cli_has_path(command: &clap::Command, path: &[&str]) -> bool {
            let Some((head, tail)) = path.split_first() else {
                return true;
            };
            let Some(sub) = command
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == *head)
            else {
                return false;
            };
            if tail.is_empty() {
                return true;
            }
            cli_has_path(sub, tail)
        }

        with_cli_root(|root| {
            let mut problems = Vec::new();
            for (area, path) in LIBRARY_AREAS {
                if !cli_has_path(&root, path) {
                    problems.push(format!(
                        "library area `{area}` has no CLI path `{}`",
                        path.join(" ")
                    ));
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    /// Every command (including hidden plumbing) has a man page in
    /// `content/docs/commands/`, and that page mentions each of its flags and
    /// subcommands. Positional arguments are named freely in the synopsis. Run `python3 scripts/docs.py` after editing the pages.
    #[test]
    fn every_command_is_documented() {
        with_cli_root(|root| {
            let pages = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/docs/commands");
            let mut problems = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" {
                    continue;
                }
                let path = pages.join(format!("{name}.md"));
                let Ok(page) = std::fs::read_to_string(&path) else {
                    problems.push(format!("missing page {}", path.display()));
                    continue;
                };
                for arg in command.get_arguments() {
                    let id = arg.get_id().as_str();
                    if GLOBAL_OPTIONS.contains(&id) {
                        continue;
                    }
                    let spellings = arg
                        .get_long()
                        .map(|long| format!("--{long}"))
                        .into_iter()
                        .chain(arg.get_short().map(|short| format!("-{short}")));
                    for spelling in spellings {
                        if !page.contains(&format!("`{spelling}")) {
                            problems.push(format!("{name}.md does not document `{spelling}`"));
                        }
                    }
                }
                for sub in command.get_subcommands() {
                    let sub_name = sub.get_name();
                    if sub_name != "help" && !page.contains(&format!("`{sub_name}")) {
                        problems.push(format!("{name}.md does not document `{sub_name}`"));
                    }
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    fn snapshot_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
    }

    fn read_snapshot_payload(stem: &str) -> Option<serde_json::Value> {
        use crate::json_schema::{discover_snapshot_stems, SNAPSHOT_FILE_PREFIX};
        let dir = snapshot_dir();
        if !discover_snapshot_stems(&dir).contains(stem) {
            return None;
        }
        let path = dir.join(format!("{SNAPSHOT_FILE_PREFIX}{stem}.snap"));
        let text = std::fs::read_to_string(&path).ok()?;
        let json_body = text.rsplit_once("\n---\n").map(|(_, body)| body)?;
        serde_json::from_str(json_body).ok()
    }

    fn mode_triple_present(value: &serde_json::Value) -> bool {
        ["human", "json", "markdown"]
            .iter()
            .all(|key| value.get(key).is_some())
    }

    /// Snapshots that are not three-mode command outcome fixtures.
    const NON_OUTCOME_SNAPSHOT_STEMS: &[&str] = &["help_output"];

    fn snapshot_has_three_modes(stem: &str, payload: &serde_json::Value) -> bool {
        if NON_OUTCOME_SNAPSHOT_STEMS.contains(&stem) {
            return payload.is_object();
        }
        if stem == "auth_render" {
            return payload.get("sign_in").is_some_and(mode_triple_present)
                && payload.get("logout").is_some_and(mode_triple_present);
        }
        mode_triple_present(payload)
    }

    fn command_covered_by_snapshot_stem(stem: &str, command: &str) -> bool {
        use crate::json_schema::snapshot_command_args;
        if RENDER_SNAPSHOT_FOR_COMMAND
            .iter()
            .any(|(cmd, snap)| *cmd == command && *snap == stem)
        {
            return true;
        }
        let Some((_, args)) = snapshot_command_args()
            .iter()
            .find(|(name, _)| *name == stem)
        else {
            return false;
        };
        args.first().is_some_and(|first| *first == command)
    }

    /// Every non-plumbing command has at least one committed three-mode output snapshot.
    #[test]
    fn every_command_has_output_snapshots() {
        use crate::json_schema::discover_snapshot_stems;
        with_cli_root(move |root| {
            let dir = snapshot_dir();
            let stems = discover_snapshot_stems(&dir);
            let mut problems = Vec::new();
            for stem in &stems {
                let Some(payload) = read_snapshot_payload(stem) else {
                    problems.push(format!("snapshot `{stem}` is unreadable"));
                    continue;
                };
                if !snapshot_has_three_modes(stem, &payload) {
                    problems.push(format!(
                        "snapshot `{stem}` must include human, json, and markdown sections"
                    ));
                }
            }
            for command in root.get_subcommands() {
                let name = command.get_name();
                if OUTPUT_SNAPSHOT_EXEMPT.contains(&name) {
                    continue;
                }
                let covered = stems
                    .iter()
                    .any(|stem| command_covered_by_snapshot_stem(stem, name));
                if !covered {
                    problems.push(format!("command `{name}` has no output snapshot"));
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    fn schema_top_level_field_names(schema: &serde_json::Value) -> HashSet<String> {
        schema_field_names_in(schema, schema)
    }

    /// Top-level properties of `node`, following `#/definitions/` refs into `root`.
    fn schema_field_names_in(
        node: &serde_json::Value,
        root: &serde_json::Value,
    ) -> HashSet<String> {
        let definition = |value: &serde_json::Value| {
            value
                .get("$ref")
                .and_then(|r| r.as_str())
                .and_then(|r| r.strip_prefix("#/definitions/"))
                .and_then(|name| root.get("definitions").and_then(|d| d.get(name)))
        };
        if let Some(def) = definition(node) {
            return schema_field_names_in(def, root);
        }
        let mut keys = HashSet::new();
        if let Some(props) = node.get("properties").and_then(|v| v.as_object()) {
            keys.extend(props.keys().cloned());
            return keys;
        }
        for branch_key in ["anyOf", "oneOf"] {
            if let Some(items) = node.get(branch_key).and_then(|v| v.as_array()) {
                for item in items {
                    keys.extend(schema_field_names_in(item, root));
                }
                return keys;
            }
        }
        if let Some(items) = node.get("items") {
            keys.extend(schema_field_names_in(items, root));
        }
        keys
    }

    fn schema_keys_for_command(command: &str) -> HashSet<String> {
        use crate::json_schema::{load_committed_schema, schema_registry};
        let mut keys = HashSet::new();
        for entry in schema_registry() {
            if entry.command == command || entry.command.starts_with(&format!("{command}.")) {
                keys.extend(schema_top_level_field_names(&load_committed_schema(
                    entry.command,
                )));
            }
        }
        keys
    }

    fn page_requires_markdown(command: &clap::Command) -> bool {
        cli_supports_markdown() || command_supports_markdown(command)
    }

    fn markdown_section_has_schema_link(section: &str, command: &str) -> bool {
        section.contains("/schemas/")
            || section.contains("grit-scm.com/schemas")
            || section.contains(&format!("/schemas/{command}/"))
    }

    /// JSON field tables document exactly the top-level schema properties; Markdown sections link schemas.
    #[test]
    fn json_field_tables_match_schemas() {
        with_cli_root(|root| {
            let dir = commands_dir();
            let mut problems = Vec::new();
            for command in root.get_subcommands() {
                let name = command.get_name();
                if name == "help" || NO_JSON_COMMANDS.contains(&name) {
                    continue;
                }
                let path = dir.join(format!("{name}.md"));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    problems.push(format!("missing page {}", path.display()));
                    continue;
                };
                let (_, body) = split_front_matter(&text);
                let Some(json_section) = section_body(body, "JSON output") else {
                    problems.push(format!("{name}.md: no JSON output section"));
                    continue;
                };
                if !json_section.contains("Schema:") && !json_section.contains("/schemas/") {
                    problems.push(format!(
                        "{name}.md: JSON output section must link the v1 schema under /schemas/"
                    ));
                }
                let table_keys = field_table_top_level_keys(&json_section);
                let schema_keys = schema_keys_for_command(name);
                if table_keys != schema_keys {
                    let missing: Vec<_> = schema_keys.difference(&table_keys).collect();
                    let extra: Vec<_> = table_keys.difference(&schema_keys).collect();
                    if !missing.is_empty() {
                        problems.push(format!(
                            "{name}.md: field table missing schema keys: {missing:?}"
                        ));
                    }
                    if !extra.is_empty() {
                        problems.push(format!(
                            "{name}.md: field table documents keys not in schema: {extra:?}"
                        ));
                    }
                }
                if page_requires_markdown(command) {
                    let Some(md_section) = section_body(body, "Markdown output") else {
                        problems.push(format!("{name}.md: missing ## Markdown output section"));
                        continue;
                    };
                    if !markdown_section_has_schema_link(&md_section, name) {
                        problems.push(format!(
                            "{name}.md: Markdown output must link the command JSON schema"
                        ));
                    }
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    #[test]
    fn render_snapshot_missing_markdown_mode_is_detected() {
        let payload = serde_json::json!({
            "sign_in": {
                "human": { "exit": 0, "stdout": "", "stderr": "" },
                "json": { "exit": 0, "stdout": "{}", "stderr": "" }
            },
            "logout": {
                "human": { "exit": 0, "stdout": "", "stderr": "" },
                "json": { "exit": 0, "stdout": "{}", "stderr": "" },
                "markdown": { "exit": 0, "stdout": "", "stderr": "" }
            }
        });
        assert!(
            !snapshot_has_three_modes("auth_render", &payload),
            "auth_render must require human, json, and markdown for every scenario"
        );
    }

    #[test]
    fn markdown_schema_link_must_live_in_markdown_section() {
        let json_section = "Schema: [v1](/schemas/pull/v1.json)\n";
        let md_section = "Pass `--markdown` for agent-friendly output.\n";
        assert!(markdown_section_has_schema_link(json_section, "pull"));
        assert!(!markdown_section_has_schema_link(md_section, "pull"));
    }

    #[cfg(unix)]
    #[test]
    fn normalize_tag_verify_preserves_raw_names_and_global_values() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(b"raw\xff".to_vec());
        let args = vec![
            "grit".into(),
            "-C".into(),
            raw.clone(),
            "tag".into(),
            "--verify".into(),
            raw.clone(),
            "--json".into(),
        ];
        assert_eq!(
            crate::args::normalize_tag_verify_argv(args),
            vec![
                "grit".into(),
                "-C".into(),
                raw.clone(),
                "tag".into(),
                "--verify".into(),
                "--json".into(),
                "--".into(),
                raw,
            ]
        );
    }

    #[test]
    fn normalize_log_limit_argv_rewrites_dash_n() {
        let out = crate::args::normalize_log_limit_argv(vec![
            "grit".into(),
            "log".into(),
            "--graph".into(),
            "--oneline".into(),
            "-3".into(),
        ]);
        assert_eq!(out, vec!["grit", "log", "--graph", "--oneline", "-n", "3"]);
    }

    #[test]
    fn normalize_log_limit_argv_skips_when_limit_present() {
        let args = vec![
            "grit".into(),
            "log".into(),
            "-n".into(),
            "2".into(),
            "-1".into(),
        ];
        assert_eq!(crate::args::normalize_log_limit_argv(args.clone()), args);
    }

    #[test]
    fn normalize_tag_verify_argv_two_names_and_globals() {
        let out = crate::args::normalize_tag_verify_argv(vec![
            "grit".into(),
            "tag".into(),
            "--verify".into(),
            "one".into(),
            "two".into(),
            "--no-signature".into(),
            "--json".into(),
        ]);
        assert_eq!(
            out,
            vec![
                "grit",
                "tag",
                "--verify",
                "--no-signature",
                "--json",
                "--",
                "one",
                "two"
            ]
        );
    }

    #[test]
    fn normalize_tag_verify_argv_preserves_filter_and_threads_values() {
        let filter = crate::args::normalize_tag_verify_argv(vec![
            "grit".into(),
            "tag".into(),
            "--verify".into(),
            "missing".into(),
            "--filter".into(),
            ".tags".into(),
            "--json".into(),
        ]);
        assert_eq!(
            filter,
            vec!["grit", "tag", "--verify", "--filter", ".tags", "--json", "--", "missing"]
        );
        let threads = crate::args::normalize_tag_verify_argv(vec![
            "grit".into(),
            "tag".into(),
            "--verify".into(),
            "missing".into(),
            "--threads".into(),
            "1".into(),
            "--json".into(),
        ]);
        assert_eq!(
            threads,
            vec![
                "grit",
                "tag",
                "--verify",
                "--threads",
                "1",
                "--json",
                "--",
                "missing"
            ]
        );
    }

    #[test]
    fn normalize_tag_verify_argv_skips_leading_repo_globals() {
        let args: Vec<std::ffi::OsString> = [
            "grit", "-C", "tag", "-c", "a.b=tag", "tag", "--verify", "v1",
        ]
        .map(std::ffi::OsString::from)
        .to_vec();
        assert_eq!(
            crate::args::normalize_tag_verify_argv(args),
            vec!["grit", "-C", "tag", "-c", "a.b=tag", "tag", "--verify", "--", "v1"]
        );
    }

    #[test]
    fn repo_globals_parse_before_the_subcommand_only() {
        crate::cli_test_stack::run(|| {
            let cli = <crate::Cli as clap::Parser>::try_parse_from([
                "grit",
                "-C",
                "a",
                "-Cb",
                "-c",
                "user.name=CI",
                "-ccore.bare",
                "--git-dir",
                "x/.git",
                "--work-tree=y",
                "status",
            ])
            .unwrap();
            assert_eq!(cli.repo.dirs, [PathBuf::from("a"), PathBuf::from("b")]);
            let config: Vec<_> = cli
                .repo
                .config
                .iter()
                .map(|o| (o.key(), o.value()))
                .collect();
            assert_eq!(config, [("user.name", Some("CI")), ("core.bare", None)]);
            assert_eq!(cli.repo.git_dir, Some(PathBuf::from("x/.git")));
            assert_eq!(cli.repo.work_tree, Some(PathBuf::from("y")));

            assert!(
                <crate::Cli as clap::Parser>::try_parse_from(["grit", "status", "-C", "a"])
                    .is_err()
            );
            let bad = <crate::Cli as clap::Parser>::try_parse_from([
                "grit",
                "-c",
                "nosection=x",
                "status",
            ])
            .unwrap_err();
            assert_eq!(bad.kind(), clap::error::ErrorKind::ValueValidation);
        });
    }

    fn commands_src_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/commands")
    }

    fn list_command_rust_sources() -> Vec<PathBuf> {
        let root = commands_src_root();
        let mut paths = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let read_dir = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}"));
            for entry in read_dir {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_some_and(|e| e == "rs") {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        paths
    }

    const COMMAND_HELPER_MODULES: &[&str] = &[
        "completions",
        "manager",
        "fetch_args",
        "merge_strategy",
        "replay_merge_args",
        "pull_outcome",
        "serve",
    ];

    fn command_stem(path: &std::path::Path) -> String {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "mod.rs" {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("mod")
                .replace('-', "_")
        } else {
            path.file_stem()
                .unwrap()
                .to_str()
                .unwrap()
                .replace('-', "_")
        }
    }

    fn render_module_stem(command_stem: &str) -> String {
        match command_stem {
            "merge_base_cmd" => "merge_base".to_string(),
            "checkin" => "checkin".to_string(),
            "serve_http" => "serve".to_string(),
            other => other.to_string(),
        }
    }

    fn read_source(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
    }

    /// Command handlers must not branch on output mode; dispatch owns that.
    #[test]
    fn commands_do_not_branch_on_output_mode() {
        let forbidden = [
            "OutputMode::",
            "opts.mode",
            "output.mode",
            "mode.is_human",
            "mode == OutputMode",
            "match opts.mode",
            "match output.mode",
            "match *mode",
        ];
        let mut problems = Vec::new();
        for path in list_command_rust_sources() {
            if path.file_name().is_some_and(|n| n == "dispatch.rs") {
                continue;
            }
            let rel = path
                .strip_prefix(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"))
                .unwrap()
                .display();
            for line in read_source(&path).lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                for marker in forbidden {
                    if line.contains(marker) {
                        problems.push(format!("{rel}: `{marker}` in `{trimmed}`"));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn commands_have_no_renderers() {
        const FORBIDDEN: &[&str] = &[
            "impl HumanRender",
            "impl MarkdownRender",
            "impl StreamRender",
            "outln!",
        ];
        let mut problems = Vec::new();
        for path in list_command_rust_sources() {
            if path.file_name().is_some_and(|n| n == "dispatch.rs") {
                continue;
            }
            let rel = path
                .strip_prefix(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"))
                .unwrap()
                .display();
            let source = read_source(&path);
            for marker in FORBIDDEN {
                if source.contains(marker) {
                    problems.push(format!("{rel} must not contain `{marker}` (use render/)"));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn every_command_has_a_renderer() {
        let render_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/render");
        let mut problems = Vec::new();
        for path in list_command_rust_sources() {
            if path.file_name().is_some_and(|n| n == "dispatch.rs") {
                continue;
            }
            let stem = command_stem(&path);
            if stem == "commands" {
                continue;
            }
            if COMMAND_HELPER_MODULES.contains(&stem.as_str()) {
                continue;
            }
            if NO_JSON_COMMANDS.contains(&stem.as_str()) {
                continue;
            }
            let rel = path
                .strip_prefix(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/commands"))
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.contains('/') {
                // Nested command modules (e.g. share/*, diff/*) share a top-level renderer.
                let top = rel.split('/').next().unwrap();
                if top == "diff" || top == "share" {
                    continue;
                }
            }
            let render_stem = render_module_stem(&stem);
            let render_path = render_root.join(format!("{render_stem}.rs"));
            if !render_path.is_file() {
                problems.push(format!("command `{stem}` has no render/{render_stem}.rs"));
                continue;
            }
            let render_src = read_source(&render_path);
            if !render_src.contains("impl HumanRender") {
                problems.push(format!(
                    "render/{render_stem}.rs must implement HumanRender for `{stem}` outcomes"
                ));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn share_command_modules_stay_within_line_budget() {
        const MAX_LINES: usize = 900;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/commands/share");
        for path in ["ops.rs", "divergence.rs", "integrated.rs", "mod.rs"] {
            let path = root.join(path);
            let lines = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
                .lines()
                .count();
            assert!(
                lines <= MAX_LINES,
                "{} has {lines} lines (max {MAX_LINES})",
                path.display()
            );
        }
    }
}
