//! Render `grit config` outcomes.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// One value in a `get_all` outcome.
#[derive(Serialize, JsonSchema, Clone)]
pub struct ConfigValueWithOrigin {
    pub value: Option<String>,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

/// Result of `grit config`, tagged by `action`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConfigOutcome {
    Get {
        key: String,
        value: Option<String>,
        scope: Option<String>,
        file: Option<String>,
        line: Option<u64>,
    },
    GetAll {
        key: String,
        values: Vec<ConfigValueWithOrigin>,
    },
    List {
        entries: Vec<ConfigEntry>,
    },
    Set {
        key: String,
        value: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    Add {
        key: String,
        value: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    Unset {
        key: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    UnsetAll {
        key: String,
        removed: u64,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    RenameSection {
        old_name: String,
        new_name: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    RemoveSection {
        section: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// One entry in a `list` outcome. `value` is `null` for a bare/valueless key.
#[derive(Serialize, JsonSchema)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Option<String>,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl HumanRender for ConfigOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ConfigOutcome::Get {
                value: Some(value), ..
            } => outln!(w, "{value}"),
            ConfigOutcome::Get { value: None, .. } => {}
            ConfigOutcome::GetAll { values, .. } => {
                for v in values {
                    if let Some(value) = &v.value {
                        outln!(w, "{value}");
                    }
                }
            }
            ConfigOutcome::List { entries } => {
                for entry in entries {
                    match &entry.value {
                        Some(value) => outln!(w, "{}={value}", entry.key),
                        None => outln!(w, "{}", entry.key),
                    }
                }
            }
            ConfigOutcome::Set {
                preview, dry_run, ..
            }
            | ConfigOutcome::Add {
                preview, dry_run, ..
            }
            | ConfigOutcome::Unset {
                preview, dry_run, ..
            }
            | ConfigOutcome::UnsetAll {
                preview, dry_run, ..
            }
            | ConfigOutcome::RenameSection {
                preview, dry_run, ..
            }
            | ConfigOutcome::RemoveSection {
                preview, dry_run, ..
            } => {
                if *dry_run {
                    if let Some(p) = preview {
                        outln!(w, "{p}");
                    }
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for ConfigOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ConfigOutcome::Get {
                key,
                value,
                scope,
                file,
                line,
            } => {
                outln!(w, "# Config `{key}`");
                outln!(w);
                match value {
                    Some(v) => outln!(w, "**Value:** `{v}`"),
                    None => outln!(w, "**Value:** _not set_"),
                }
                if let Some(scope) = scope {
                    outln!(w, "**Scope:** `{scope}`");
                }
                if let Some(file) = file {
                    outln!(w, "**File:** `{file}`");
                }
                if let Some(line) = line {
                    outln!(w, "**Line:** {line}");
                }
            }
            ConfigOutcome::GetAll { key, values } => {
                outln!(w, "# Config `{key}` (all values)");
                outln!(w);
                for v in values {
                    outln!(
                        w,
                        "- `{}` ({})",
                        v.value.as_deref().unwrap_or("true"),
                        v.scope
                    );
                }
            }
            ConfigOutcome::List { entries } => markdown::print_config_entries(w, entries)?,
            ConfigOutcome::Set {
                key,
                value,
                dry_run,
                preview,
            }
            | ConfigOutcome::Add {
                key,
                value,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: would set `{key}` = {value}. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Set `{key}` = {value}.");
                }
            }
            ConfigOutcome::Unset {
                key,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: would unset `{key}`. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Unset `{key}`.");
                }
            }
            ConfigOutcome::UnsetAll {
                key,
                removed,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: would unset-all `{key}` ({removed} values). {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Unset all `{key}` ({removed} values).");
                }
            }
            ConfigOutcome::RenameSection {
                old_name,
                new_name,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: rename `{old_name}` → `{new_name}`. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Renamed section `{old_name}` → `{new_name}`.");
                }
            }
            ConfigOutcome::RemoveSection {
                section,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: remove section `{section}`. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Removed section `{section}`.");
                }
            }
        }
        Ok(())
    }
}
