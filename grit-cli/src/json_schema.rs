//! v1 JSON Schema generation and registry for `--json` command outcomes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use schemars::schema::RootSchema;
use schemars::{schema_for, JsonSchema};

use crate::commands;
use crate::json_error::StructuredJsonError;
use serde::Serialize;

/// Any `--json` failure object on stdout (schema contract only; see `error.v1.json`).
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum JsonErrorEnvelope {
    /// Usual command failure (`{"error":"…"}` only).
    Plain {
        /// Same text a human sees after `error:` on stderr.
        error: String,
    },
    /// Typed failure (conflicts, …).
    Structured(StructuredJsonError),
}

/// One row in [`schema_registry`]: command path and its success-outcome type.
pub struct SchemaRegistryEntry {
    /// Dotted command path (`stash.list`, `remote.refs`, …).
    pub command: &'static str,
    /// Generates a JSON Schema for the command's success `--json` object.
    pub schema: fn() -> RootSchema,
}

/// Commands whose stdout is not a JSON outcome object (plumbing / raw protocols).
pub const NO_JSON_COMMANDS: &[&str] = &["completions", "manager", "upload-pack", "receive-pack"];

/// Relative path from the workspace root to committed schema files.
pub fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/schemas")
}

/// Published schema index at `content/docs/schemas.md`.
pub fn schemas_index_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/docs/schemas.md")
}

/// Every committed schema document: command path → version numbers present on disk.
pub fn discover_committed_schema_documents() -> BTreeMap<String, BTreeSet<u32>> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut out: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let dir = schemas_dir();
    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("read schemas dir: {e}")) {
        let entry = entry.unwrap_or_else(|e| panic!("schema dir entry: {e}"));
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((command, version)) = name.strip_suffix(".json").and_then(|stem| {
            stem.rsplit_once(".v")
                .and_then(|(cmd, ver)| ver.parse::<u32>().ok().map(|v| (cmd.to_owned(), v)))
        }) else {
            continue;
        };
        out.entry(command).or_default().insert(version);
    }
    out
}

/// Parse `/schemas/<command>/vN.json` links from the schema index table body.
pub fn parse_schemas_index_links(markdown: &str) -> BTreeMap<String, BTreeSet<u32>> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut out: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for line in markdown.lines() {
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let Some((command, links_cell)) = rest.split_once("` | ") else {
            continue;
        };
        let links = links_cell.strip_suffix(" |").unwrap_or(links_cell);
        for link in links.split(',') {
            let Some(url) = link
                .trim()
                .strip_prefix('[')
                .and_then(|s| s.split_once("]("))
                .map(|(_, url)| url.trim_end_matches(')'))
            else {
                continue;
            };
            let Some(path) = url.strip_prefix("/schemas/") else {
                continue;
            };
            let Some((cmd, ver)) = path.strip_suffix(".json").and_then(|p| {
                p.rsplit_once("/v")
                    .and_then(|(c, v)| v.parse::<u32>().ok().map(|n| (c.to_owned(), n)))
            }) else {
                continue;
            };
            assert_eq!(
                cmd, command,
                "schema index row `{command}` links to `/schemas/{cmd}/`"
            );
            out.entry(command.to_owned()).or_default().insert(ver);
        }
    }
    out
}

/// Stable `$id` URL for a command's v1 schema document.
pub fn schema_id(command: &str) -> String {
    format!("https://grit-scm.com/schemas/{command}/v1.json")
}

/// Stable `$id` URL for a command's v2 schema document.
pub fn schema_id_v2(command: &str) -> String {
    format!("https://grit-scm.com/schemas/{command}/v2.json")
}

/// Commands whose live `--json` shape is versioned separately from the frozen v1 contract.
const SCHEMA_V2_COMMANDS: &[&str] = &["merge", "pull", "pick", "mv"];

/// File name for a command schema under [`schemas_dir`].
pub fn schema_file_name(command: &str) -> String {
    format!("{command}.v1.json")
}

/// File name for a v2 schema document.
pub fn schema_file_name_v2(command: &str) -> String {
    format!("{command}.v2.json")
}

fn build_schema_registry() -> Vec<SchemaRegistryEntry> {
    use commands::auth::LogoutOutcome;
    use commands::{
        add::AddOutcome,
        auth::AuthOutcome,
        blame::BlameOutcome,
        branch::BranchOutcome,
        bundle::{
            BundleCreateOutcome, BundleListOutcome, BundleUnbundleOutcome, BundleVerifyOutcome,
        },
        cat::CatOutcome,
        clean::CleanOutcome,
        clone::CloneOutcome,
        commit::CommitOutcome,
        config::ConfigOutcome,
        diff::DiffOutcome,
        fetch::{FetchAllOutcome, FetchOutcome},
        fsck::FsckOutcome,
        gc::GcOutcome,
        history::{
            HistoryAbsorbOutcome, HistoryRewriteOutcome, RebaseCheckOutcome, RebaseCliOutcome,
        },
        hooks::HooksOutcome,
        ignore::IgnoreCheckOutcome,
        init::InitOutcome,
        log::LogOutcome,
        merge::MergeOutcome,
        merge_base_cmd::MergeBaseOutcome,
        mv::MvOutcome,
        notes::NotesOutcome,
        patch_id::PatchIdOutcome,
        pick::PickOutcome,
        pull::PullOutcome,
        push::PushOutcome,
        reflog::{ReflogDeleteOutcome, ReflogExpireOutcome, ReflogListOutcome},
        remote::RemoteOutcome,
        repack::RepackOutcome,
        reset::ResetCliOutcome,
        resolve::ResolveOutcome,
        restore::RestoreCliOutcome,
        revert::RevertOutcome,
        revs::RevsOutcome,
        rm::RmOutcome,
        shortlog::ShortlogOutcome,
        show::ShowRunResult,
        skill::SkillOutcome,
        stash::{
            StashApplyOutcome, StashBranchOutcome, StashClearOutcome, StashDropOutcome,
            StashListOutcome, StashPopOutcome, StashPushOutcome, StashShowOutcome,
        },
        status::StatusOutcome,
        switch::SwitchOutcome,
        tag::TagOutcome,
        update::UpdateOutcome,
        worktree::WorktreeOutcome,
    };

    vec![
        entry::<StatusOutcome>("status"),
        entry::<InitOutcome>("init"),
        entry::<CloneOutcome>("clone"),
        entry::<RemoteOutcome>("remote"),
        entry::<RemoteOutcome>("remote.add"),
        entry::<RemoteOutcome>("remote.set-url"),
        entry::<RemoteOutcome>("remote.refs"),
        entry::<RemoteOutcome>("remote.remove"),
        entry::<LogOutcome>("log"),
        entry::<RevsOutcome>("revs"),
        entry::<MergeBaseOutcome>("merge-base"),
        entry::<ReflogListOutcome>("reflog"),
        entry::<ReflogDeleteOutcome>("reflog.delete"),
        entry::<ReflogExpireOutcome>("reflog.expire"),
        entry::<BlameOutcome>("blame"),
        entry::<DiffOutcome>("diff"),
        entry::<ShowRunResult>("show"),
        entry::<ShortlogOutcome>("shortlog"),
        entry::<AddOutcome>("add"),
        entry::<RmOutcome>("rm"),
        entry::<MvOutcome>("mv"),
        entry::<CleanOutcome>("clean"),
        entry::<RepackOutcome>("repack"),
        entry::<GcOutcome>("gc"),
        entry::<GcOutcome>("gc.repack"),
        entry::<GcOutcome>("gc.commit-graph"),
        entry::<GcOutcome>("gc.midx"),
        entry::<GcOutcome>("gc.pack-refs"),
        entry::<GcOutcome>("gc.prune"),
        entry::<FsckOutcome>("fsck"),
        entry::<HooksOutcome>("hooks"),
        entry::<HooksOutcome>("hooks.list"),
        entry::<HooksOutcome>("hooks.run"),
        entry::<IgnoreCheckOutcome>("ignore.check"),
        entry::<PatchIdOutcome>("patch-id"),
        entry::<ResolveOutcome>("resolve"),
        entry::<CatOutcome>("cat"),
        entry::<CommitOutcome>("commit"),
        entry::<BranchOutcome>("branch"),
        entry::<SwitchOutcome>("switch"),
        entry::<ResetCliOutcome>("reset"),
        entry::<MergeOutcome>("merge"),
        entry::<NotesOutcome>("notes"),
        entry::<NotesOutcome>("notes.list"),
        entry::<NotesOutcome>("notes.show"),
        entry::<NotesOutcome>("notes.add"),
        entry::<NotesOutcome>("notes.remove"),
        entry::<PickOutcome>("pick"),
        entry::<HistoryRewriteOutcome>("history.reword"),
        entry::<HistoryRewriteOutcome>("history.drop"),
        entry::<HistoryRewriteOutcome>("history.move"),
        entry::<HistoryRewriteOutcome>("history.fixup"),
        entry::<HistoryRewriteOutcome>("history.squash"),
        entry::<RebaseCliOutcome>("history.rebase"),
        entry::<RebaseCliOutcome>("rebase"),
        entry::<HistoryRewriteOutcome>("history.split"),
        entry::<RebaseCheckOutcome>("rebase.check"),
        entry::<HistoryAbsorbOutcome>("history.absorb"),
        entry::<HistoryRewriteOutcome>("history.undo"),
        entry::<RevertOutcome>("revert"),
        entry::<StashPushOutcome>("stash"),
        entry::<StashPushOutcome>("stash.push"),
        entry::<StashListOutcome>("stash.list"),
        entry::<StashShowOutcome>("stash.show"),
        entry::<StashApplyOutcome>("stash.apply"),
        entry::<StashPopOutcome>("stash.pop"),
        entry::<StashDropOutcome>("stash.drop"),
        entry::<StashBranchOutcome>("stash.branch"),
        entry::<StashClearOutcome>("stash.clear"),
        entry::<RestoreCliOutcome>("restore"),
        entry::<FetchOutcome>("fetch"),
        entry::<FetchAllOutcome>("fetch.all"),
        entry::<PullOutcome>("pull"),
        entry::<PushOutcome>("push"),
        entry::<BundleCreateOutcome>("bundle.create"),
        entry::<BundleVerifyOutcome>("bundle.verify"),
        entry::<BundleListOutcome>("bundle.list"),
        entry::<BundleUnbundleOutcome>("bundle.unbundle"),
        entry::<TagOutcome>("tag"),
        entry::<WorktreeOutcome>("worktree.list"),
        entry::<WorktreeOutcome>("worktree.add"),
        entry::<WorktreeOutcome>("worktree.remove"),
        entry::<WorktreeOutcome>("worktree.prune"),
        entry::<WorktreeOutcome>("worktree.lock"),
        entry::<WorktreeOutcome>("worktree.unlock"),
        entry::<AuthOutcome>("auth"),
        entry::<LogoutOutcome>("auth.logout"),
        entry::<UpdateOutcome>("update"),
        entry::<SkillOutcome>("skill"),
        entry::<ConfigOutcome>("config"),
        entry::<JsonErrorEnvelope>("error"),
    ]
}

static SCHEMA_REGISTRY: OnceLock<Vec<SchemaRegistryEntry>> = OnceLock::new();

/// Every non-plumbing command path and the Rust type that defines its `--json` success shape.
pub fn schema_registry() -> &'static [SchemaRegistryEntry] {
    SCHEMA_REGISTRY
        .get_or_init(build_schema_registry)
        .as_slice()
}

const fn entry<T: JsonSchema>(command: &'static str) -> SchemaRegistryEntry {
    SchemaRegistryEntry {
        command,
        schema: schema_for_type::<T>,
    }
}

fn schema_for_type<T: JsonSchema>() -> RootSchema {
    schema_for!(T)
}

fn args_has_flag(args: &[&str], flag: &str) -> bool {
    args.iter().copied().any(|a| a == flag)
}

/// Resolve the schema registry key for a `grit` argv slice (no global flags).
pub fn schema_command_key(args: &[&str]) -> Option<String> {
    let positional: Vec<&str> = args
        .iter()
        .copied()
        .filter(|a| !a.starts_with('-'))
        .collect();
    if positional.is_empty() {
        return Some("status".into());
    }
    let key = match positional[0] {
        "remote" => match positional.get(1).copied() {
            Some("add") => "remote.add",
            Some("set-url") => "remote.set-url",
            Some("refs") => "remote.refs",
            Some("remove") => "remote.remove",
            _ => "remote",
        },
        "reflog" => match positional.get(1).copied() {
            None => "reflog",
            Some("delete") => "reflog.delete",
            Some("expire") => "reflog.expire",
            _ => return None,
        },
        "stash" => match positional.get(1).copied() {
            None | Some("push") => {
                if positional.len() == 1 {
                    "stash"
                } else {
                    "stash.push"
                }
            }
            Some("list") => "stash.list",
            Some("show") => "stash.show",
            Some("apply") => "stash.apply",
            Some("pop") => "stash.pop",
            Some("drop") => "stash.drop",
            Some("branch") => "stash.branch",
            Some("clear") => "stash.clear",
            _ => return None,
        },
        "bundle" => match positional.get(1).copied() {
            Some("create") => "bundle.create",
            Some("verify") => "bundle.verify",
            Some("list") => "bundle.list",
            Some("unbundle") => "bundle.unbundle",
            _ => return None,
        },
        "worktree" => match positional.get(1).copied() {
            Some("list") => "worktree.list",
            Some("add") => "worktree.add",
            Some("remove") => "worktree.remove",
            Some("prune") => "worktree.prune",
            Some("lock") => "worktree.lock",
            Some("unlock") => "worktree.unlock",
            _ => return None,
        },
        "auth" => match positional.get(1).copied() {
            Some("logout") => "auth.logout",
            None => "auth",
            _ => return None,
        },
        "ignore" => match positional.get(1).copied() {
            Some("check") => "ignore.check",
            _ => return None,
        },
        "notes" => match positional.get(1).copied() {
            None => "notes",
            Some("list") => "notes.list",
            Some("show") => "notes.show",
            Some("add") => "notes.add",
            Some("remove") => "notes.remove",
            _ => return None,
        },
        "hooks" => match positional.get(1).copied() {
            None => "hooks",
            Some("list") => "hooks.list",
            Some("run") => "hooks.run",
            _ => return None,
        },
        "fetch" if args_has_flag(args, "--all") => "fetch.all",
        "rebase" if args_has_flag(args, "--check") => "rebase.check",
        "gc" => match positional.get(1).copied() {
            None => "gc",
            Some("repack") => "gc.repack",
            Some("commit-graph") => "gc.commit-graph",
            Some("midx") => "gc.midx",
            Some("pack-refs") => "gc.pack-refs",
            Some("prune") => "gc.prune",
            _ => return None,
        },
        "history" => match positional.get(1).copied() {
            Some("reword") => "history.reword",
            Some("drop") => "history.drop",
            Some("move") => "history.move",
            Some("fixup") => "history.fixup",
            Some("squash") => "history.squash",
            Some("split") => "history.split",
            Some("absorb") => "history.absorb",
            Some("rebase") => "history.rebase",
            Some("undo") => "history.undo",
            _ => return None,
        },
        "rebase" => "rebase",
        cmd if NO_JSON_COMMANDS.contains(&cmd) => return None,
        cmd => cmd,
    };
    Some(key.to_owned())
}

/// Load the committed schema document for `command` (v1, or v2 when listed in [`SCHEMA_V2_COMMANDS`]).
pub fn load_committed_schema(command: &str) -> serde_json::Value {
    let path = if SCHEMA_V2_COMMANDS.contains(&command) {
        schemas_dir().join(schema_file_name_v2(command))
    } else {
        schemas_dir().join(schema_file_name(command))
    };
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read schema {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse schema {}: {e}", path.display()))
}

/// Regenerate every schema file under [`schemas_dir`].
pub fn write_all_schemas(dir: &Path) {
    if let Err(e) = fs::create_dir_all(dir) {
        panic!("create schemas dir: {e}");
    }
    for entry in schema_registry() {
        if SCHEMA_V2_COMMANDS.contains(&entry.command) {
            let schema = (entry.schema)();
            let json = root_schema_to_document_v2(entry.command, schema);
            let path = dir.join(schema_file_name_v2(entry.command));
            let text = match serde_json::to_string_pretty(&json) {
                Ok(t) => t,
                Err(e) => panic!("serialize schema: {e}"),
            };
            if let Err(e) = fs::write(&path, format!("{text}\n")) {
                panic!("write schema {}: {e}", path.display());
            }
            continue;
        }
        let schema = (entry.schema)();
        let json = root_schema_to_document(entry.command, schema);
        let path = dir.join(schema_file_name(entry.command));
        let text = match serde_json::to_string_pretty(&json) {
            Ok(t) => t,
            Err(e) => panic!("serialize schema: {e}"),
        };
        if let Err(e) = fs::write(&path, format!("{text}\n")) {
            panic!("write schema {}: {e}", path.display());
        }
    }
}

fn root_schema_to_document_v2(command: &str, root: RootSchema) -> serde_json::Value {
    let mut value = root_schema_to_document(command, root);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("$id".into(), schema_id_v2(command).into());
    }
    value
}

fn root_schema_to_document(command: &str, root: RootSchema) -> serde_json::Value {
    let mut value = match serde_json::to_value(&root) {
        Ok(v) => v,
        Err(e) => panic!("schema to json: {e}"),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "$schema".into(),
            "https://json-schema.org/draft/2020-12/schema".into(),
        );
        obj.insert("$id".into(), schema_id(command).into());
    }
    if command.starts_with("worktree.") {
        ensure_worktree_row_required_nullable_lock_reason(&mut value);
    }
    if command == "history.reword"
        || command == "history.drop"
        || command == "history.move"
        || command == "history.fixup"
        || command == "history.squash"
        || command == "history.split"
        || command == "history.absorb"
        || command == "history.rebase"
        || command == "rebase"
        || command == "history.undo"
    {
        ensure_required_nullable_head(&mut value);
    }
    canonicalize_json(&mut value);
    value
}

fn ensure_required_nullable_field(schema: &mut serde_json::Value, field: &str) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let Some(props) = obj.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    if !props.contains_key(field) {
        return;
    }
    let required = obj
        .entry("required")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let Some(arr) = required.as_array_mut() else {
        return;
    };
    if !arr.iter().any(|v| v.as_str() == Some(field)) {
        arr.push(serde_json::Value::String(field.into()));
        arr.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
}

/// `WorktreeRow.lock_reason` is always present in list JSON (null when absent).
fn ensure_worktree_row_required_nullable_lock_reason(schema: &mut serde_json::Value) {
    let Some(defs) = schema
        .get_mut("definitions")
        .and_then(|d| d.as_object_mut())
    else {
        return;
    };
    let Some(row) = defs.get_mut("WorktreeRow") else {
        return;
    };
    ensure_required_nullable_field(row, "lock_reason");
}

/// `schemars` omits `Option` fields from `required`; stable `--json` always emits `head`.
fn ensure_required_nullable_head(schema: &mut serde_json::Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let Some(props) = obj.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    if props.contains_key("head") {
        ensure_required_nullable_field(schema, "head");
    }
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let taken = std::mem::take(map);
            let mut sorted: BTreeMap<_, _> = taken.into_iter().collect();
            for v in sorted.values_mut() {
                canonicalize_json(v);
            }
            *map = sorted.into_iter().collect();
        }
        serde_json::Value::Array(items) => {
            for item in items {
                canonicalize_json(item);
            }
        }
        _ => {}
    }
}

/// Enumerate every non-plumbing command path that must appear in [`schema_registry`].
pub fn expected_json_command_paths() -> Vec<String> {
    crate::cli_test_stack::run(|| {
        use clap::CommandFactory;

        let mut paths = vec!["status".to_owned()];
        let root = crate::Cli::command();
        for sub in root.get_subcommands().filter(|c| c.get_name() != "help") {
            collect_json_commands(sub, None, &mut paths);
        }
        paths.sort();
        paths.dedup();
        paths
    })
}

fn collect_json_commands(cmd: &clap::Command, prefix: Option<&str>, out: &mut Vec<String>) {
    let name = cmd.get_name();
    if name == "help" {
        return;
    }
    let full = match prefix {
        None => name.to_string(),
        Some(p) => format!("{p}.{name}"),
    };
    if NO_JSON_COMMANDS.contains(&full.as_str()) || NO_JSON_COMMANDS.contains(&name) {
        return;
    }

    let subs: Vec<_> = cmd
        .get_subcommands()
        .filter(|s| s.get_name() != "help")
        .collect();

    if subs.is_empty() {
        out.push(full);
        return;
    }

    // Parent commands that run with no subcommand still emit JSON (remote list, stash push, auth login).
    if matches!(
        full.as_str(),
        "remote" | "stash" | "auth" | "hooks" | "reflog" | "gc"
    ) {
        out.push(full.clone());
    }

    for sub in subs {
        collect_json_commands(sub, Some(&full), out);
    }
}

/// Insta snapshot file prefix under `grit-cli/tests/snapshots/`.
pub const SNAPSHOT_FILE_PREFIX: &str = "grit_test_support__cli_fixture__";

/// Snapshot stems skipped by [`json_snapshots_match_schemas`] and the reason why.
pub const SNAPSHOT_JSON_SCHEMA_SKIP_REASONS: &[(&str, &str)] = &[
    (
        "auth_render",
        "In-process auth render fixture; JSON bundles sign-in and logout envelopes.",
    ),
    (
        "log_json_filter_commits_oid",
        "`--json --filter` stdout is a filtered slice, not a full command outcome object.",
    ),
    (
        "update_render",
        "In-process update render fixture; not produced by a single argv invocation.",
    ),
];

/// Snapshot stems whose `json.stdout` is not a single command outcome object (render
/// fixtures, jq filter slices, and similar). Skipped by [`json_snapshots_match_schemas`] only;
/// output snapshot coverage still requires human/json/markdown sections for render stems.
pub const SNAPSHOT_JSON_SCHEMA_SKIP: &[&str] = &[
    "auth_render",
    "log_json_filter_commits_oid",
    "update_render",
];

/// Snapshot stems (insta name) for every `*.snap` in `dir`.
pub fn discover_snapshot_stems(dir: &Path) -> std::collections::BTreeSet<String> {
    use std::collections::BTreeSet;

    let mut stems = BTreeSet::new();
    for entry in
        fs::read_dir(dir).unwrap_or_else(|e| panic!("read snapshot dir {}: {e}", dir.display()))
    {
        let entry = entry.unwrap_or_else(|e| panic!("snapshot dir entry: {e}"));
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name
            .strip_prefix(SNAPSHOT_FILE_PREFIX)
            .and_then(|s| s.strip_suffix(".snap"))
        else {
            continue;
        };
        stems.insert(stem.to_owned());
    }
    stems
}

/// Assert [`snapshot_command_args`] lists exactly every on-disk snapshot stem, except [`SNAPSHOT_JSON_SCHEMA_SKIP`].
pub fn assert_snapshot_args_cover_disk(dir: &Path) {
    use std::collections::BTreeSet;

    let on_disk = discover_snapshot_stems(dir);
    let table: BTreeSet<_> = snapshot_command_args()
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    let skip: BTreeSet<_> = SNAPSHOT_JSON_SCHEMA_SKIP
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

    for stem in &on_disk {
        if skip.contains(stem) {
            continue;
        }
        assert!(
            table.contains(stem),
            "snapshot `{stem}` exists on disk but is missing from snapshot_command_args(); add a row or SNAPSHOT_JSON_SCHEMA_SKIP if intentional"
        );
    }
    for stem in &table {
        assert!(
            on_disk.contains(stem),
            "snapshot_command_args() lists `{stem}` but no {SNAPSHOT_FILE_PREFIX}{stem}.snap file exists"
        );
    }
}

fn snapshot_args_lookup(name: &str) -> Option<&'static [&'static str]> {
    snapshot_command_args()
        .iter()
        .find_map(|(stem, args)| (*stem == name).then_some(*args))
}

/// Map snapshot stem to grit argv used to produce it (for schema validation).
pub fn snapshot_command_args() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("skill", &["skill"]),
        ("fsck", &["fsck"]),
        ("fsck_missing_blob", &["fsck"]),
        ("gc", &["gc", "--dry-run"]),
        ("gc_repack", &["gc", "repack"]),
        ("gc_commit_graph", &["gc", "commit-graph"]),
        ("gc_midx", &["gc", "midx"]),
        ("gc_pack_refs", &["gc", "pack-refs"]),
        ("gc_prune", &["gc", "prune"]),
        ("repack", &["repack", "--dry-run"]),
        ("status", &["status"]),
        ("log", &["log"]),
        ("log_oneline", &["log", "--oneline", "--limit", "3"]),
        (
            "log_decorate",
            &["log", "--decorate", "--oneline", "--limit", "1"],
        ),
        (
            "log_graph_linear",
            &["log", "--graph", "--oneline", "-n", "3"],
        ),
        (
            "log_graph_merge",
            &["log", "--graph", "--oneline", "-n", "5"],
        ),
        ("reflog", &["reflog"]),
        ("diff", &["diff"]),
        ("diff_staged", &["diff", "--staged"]),
        ("show_head", &["show", "HEAD"]),
        ("show_unknown_revision", &["show", "not-a-ref"]),
        ("commit", &["commit", "-m", "third"]),
        (
            "commit_amend",
            &["commit", "--amend", "-m", "third (amended)"],
        ),
        ("add", &["add", "tracked.txt"]),
        ("switch", &["switch", "feature"]),
        ("switch_create", &["switch", "--create", "topic"]),
        ("merge_ff", &["merge", "main"]),
        ("merge_conflict", &["merge", "side"]),
        ("pick", &["pick", "topic"]),
        (
            "history_reword",
            &["history", "reword", "HEAD~1", "-m", "reworded subject"],
        ),
        ("history_drop", &["history", "drop", "HEAD~1", "--dry-run"]),
        (
            "history_squash",
            &["history", "squash", "HEAD~2..HEAD", "--update-refs", "head"],
        ),
        ("rebase", &["rebase", "HEAD~2", "--dry-run"]),
        ("revert", &["revert", "HEAD"]),
        ("shortlog", &["shortlog"]),
        ("revs_head", &["revs", "HEAD"]),
        ("revs_count", &["revs", "--count", "HEAD"]),
        ("merge_base", &["merge-base", "HEAD~1", "HEAD"]),
        (
            "merge_base_is_ancestor",
            &["merge-base", "--is-ancestor", "HEAD~1", "HEAD"],
        ),
        ("blame_tracked", &["blame", "tracked.txt"]),
        (
            "blame_tracked_line_range",
            &["blame", "tracked.txt", "-L", "1,2"],
        ),
        ("branch_list", &["branch"]),
        ("branch_create", &["branch", "wip"]),
        ("branch_delete", &["branch", "-d", "delete-me"]),
        ("tag_list", &["tag"]),
        ("tag_lightweight", &["tag", "v1.0"]),
        ("tag_annotated", &["tag", "-m", "release note", "v2.0"]),
        ("tag_delete", &["tag", "-d", "drop-me"]),
        ("notes", &["notes"]),
        ("notes_list", &["notes", "list"]),
        ("hooks", &["hooks"]),
        ("stash", &["stash"]),
        ("notes_show", &["notes", "show", "HEAD"]),
        ("notes_add", &["notes", "add", "HEAD", "-m", "hello notes"]),
        ("notes_remove", &["notes", "remove", "HEAD"]),
        ("config_get_unset", &["config", "user.email"]),
        ("config_set", &["config", "user.email", "snap@example.com"]),
        ("config_list", &["config", "--list"]),
        ("config_unset", &["config", "--unset", "user.email"]),
        ("remote_list", &["remote"]),
        (
            "remote_add",
            &["remote", "add", "upstream", "/tmp/upstream.git"],
        ),
        (
            "remote_set_url",
            &["remote", "set-url", "origin", "https://example.com/new.git"],
        ),
        (
            "bundle_create",
            &["bundle", "create", "snap.bundle", "HEAD"],
        ),
        ("bundle_verify", &["bundle", "verify", "snap.bundle"]),
        (
            "bundle_verify_invalid",
            &["bundle", "verify", "invalid.bundle"],
        ),
        ("bundle_list", &["bundle", "list", "snap.bundle"]),
        ("bundle_unbundle", &["bundle", "unbundle", "snap.bundle"]),
        ("init", &["init"]),
        ("init_bare", &["init", "--bare"]),
        ("rm", &["rm", "second.txt"]),
        ("rm_cached", &["rm", "--cached", "second.txt"]),
        ("mv", &["mv", "second.txt", "renamed.txt"]),
        ("clean_dry_run", &["clean"]),
        ("clean_force", &["clean", "--force"]),
        ("reset_soft", &["reset", "--soft", "HEAD~1"]),
        ("reset_mixed", &["reset", "--mixed", "HEAD~1"]),
        ("reset_hard", &["reset", "--hard", "HEAD~1"]),
        ("restore_worktree", &["restore", "tracked.txt"]),
        ("restore_staged", &["restore", "--staged", "staged.txt"]),
        ("resolve_head", &["resolve", "HEAD", "HEAD^{tree}"]),
        ("resolve_short", &["resolve", "--short", "HEAD"]),
        ("cat_blob", &["cat", "HEAD:tracked.txt"]),
        ("cat_commit", &["cat", "HEAD^{commit}"]),
        ("clone_local", &["clone", "UPSTREAM", "cloned"]),
        (
            "clone_shallow",
            &["clone", "--shallow", "UPSTREAM", "cloned"],
        ),
        ("fetch_noop", &["fetch"]),
        ("fetch_new", &["fetch"]),
        ("fetch_prune", &["fetch", "--prune"]),
        ("pull_ff", &["pull"]),
        ("push_ok", &["push"]),
        ("push_up_to_date", &["push"]),
        ("push_rejected", &["push"]),
        ("push_tags", &["push", "--tags"]),
        (
            "push_delete",
            &["push", "--delete", "old-topic", "--remote", "origin"],
        ),
        (
            "remote_refs_heads",
            &["remote", "refs", "origin", "--heads"],
        ),
        ("remote_refs_tags", &["remote", "refs", "origin", "--tags"]),
        ("stash_push", &["stash", "push", "-m", "wip"]),
        ("stash_list", &["stash", "list"]),
        ("stash_show", &["stash", "show"]),
        ("stash_apply", &["stash", "apply"]),
        ("stash_pop", &["stash", "pop"]),
        ("stash_drop", &["stash", "drop"]),
        ("stash_branch", &["stash", "branch", "from-stash"]),
        ("stash_clear", &["stash", "clear", "--force"]),
        ("stash_clear_preview", &["stash", "clear"]),
        ("ignore_check", &["ignore", "check", "noise.log"]),
        (
            "ignore_check_all",
            &["ignore", "check", "--all", "noise.log", "keep.txt"],
        ),
        ("ignore_check_json", &["ignore", "check", "keep.txt"]),
        ("worktree_list", &["worktree", "list"]),
        (
            "worktree_add",
            &["worktree", "add", "linked-wt", "-b", "wt-new"],
        ),
        (
            "worktree_remove",
            &["worktree", "remove", "linked-wt", "-f"],
        ),
        (
            "worktree_lock",
            &["worktree", "lock", "linked-wt", "--reason", "hold"],
        ),
        ("worktree_unlock", &["worktree", "unlock", "linked-wt"]),
        ("hooks_list", &["hooks", "list"]),
        ("hooks_run", &["hooks", "run", "pre-commit"]),
        ("hooks_run_exit_17", &["hooks", "run", "pre-commit"]),
        ("patch_id", &["patch-id", "HEAD"]),
        ("auth_logout", &["auth", "logout"]),
        ("fetch_all", &["fetch", "--all"]),
        (
            "history_move",
            &[
                "history",
                "move",
                "HEAD~1",
                "--before",
                "HEAD~2",
                "--dry-run",
            ],
        ),
        ("history_fixup", &["history", "fixup", "HEAD~1"]),
        ("history_absorb", &["history", "absorb", "--dry-run"]),
        (
            "history_rebase",
            &["history", "rebase", "HEAD~2", "--dry-run"],
        ),
        (
            "history_split",
            &[
                "history",
                "split",
                "HEAD~1",
                "-m",
                "split part",
                "--",
                "tracked.txt",
            ],
        ),
        ("history_undo", &["history", "undo", "--dry-run"]),
        ("rebase_check", &["rebase", "HEAD~2", "--check"]),
        ("reflog_delete", &["reflog", "delete", "HEAD@{1}"]),
        ("reflog_expire", &["reflog", "expire", "--expire", "now"]),
        ("worktree_prune", &["worktree", "prune"]),
        ("error_usage", &["branch", "--delete"]),
        ("error_invalid_pattern", &["log", "--author", "["]),
        ("error_would_overwrite", &["switch", "br"]),
        ("error_conflict", &["history", "drop", "HEAD~1"]),
        ("auth_render", &[]),
        ("update_render", &[]),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::env;
    use std::fs;

    use serde_json::Value;

    use super::*;

    #[test]
    fn json_schemas_are_current() {
        let dir = schemas_dir();
        if env::var("GRIT_UPDATE_SCHEMAS").as_deref() == Ok("1") {
            write_all_schemas(&dir);
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        write_all_schemas(tmp.path());
        for entry in schema_registry() {
            if SCHEMA_V2_COMMANDS.contains(&entry.command) {
                let committed = dir.join(schema_file_name_v2(entry.command));
                let generated = tmp.path().join(schema_file_name_v2(entry.command));
                let committed_text = fs::read_to_string(&committed)
                    .unwrap_or_else(|e| panic!("read {}: {e}", committed.display()));
                let generated_text = fs::read_to_string(&generated)
                    .unwrap_or_else(|e| panic!("read {}: {e}", generated.display()));
                assert_eq!(
                    committed_text, generated_text,
                    "v2 schema drift for `{}`; run GRIT_UPDATE_SCHEMAS=1 cargo test json_schemas_are_current -p grit-cli",
                    entry.command
                );
                continue;
            }
            let committed = dir.join(schema_file_name(entry.command));
            let generated = tmp.path().join(schema_file_name(entry.command));
            let committed_text = fs::read_to_string(&committed)
                .unwrap_or_else(|e| panic!("read {}: {e}", committed.display()));
            let generated_text = fs::read_to_string(&generated)
                .unwrap_or_else(|e| panic!("read {}: {e}", generated.display()));
            assert_eq!(
                committed_text, generated_text,
                "schema drift for `{}`; run GRIT_UPDATE_SCHEMAS=1 cargo test json_schemas_are_current -p grit-cli",
                entry.command
            );
        }
    }

    #[test]
    fn schema_registry_covers_all_commands() {
        const STACK: usize = 16 * 1024 * 1024;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(|| {
                let expected = expected_json_command_paths();
                let registered: std::collections::BTreeSet<_> =
                    schema_registry().iter().map(|e| e.command).collect();
                let mut missing = Vec::new();
                for cmd in expected {
                    if cmd == "error" {
                        continue;
                    }
                    if !registered.contains(cmd.as_str()) {
                        missing.push(cmd);
                    }
                }
                assert!(
                    missing.is_empty(),
                    "schema_registry missing commands: {missing:?}"
                );
            })
            .expect("spawn schema registry audit")
            .join()
            .expect("schema registry audit thread");
    }

    fn validate_json_against_schema(name: &str, schema_key: &str, instance: &Value) {
        let schema_value = load_committed_schema(schema_key);
        let compiled = jsonschema::validator_for(&schema_value)
            .unwrap_or_else(|e| panic!("compile schema for {schema_key}: {e}"));
        if !compiled.is_valid(instance) {
            let msgs: Vec<_> = compiled
                .iter_errors(instance)
                .map(|e| e.to_string())
                .collect();
            panic!(
                "snapshot {name} JSON does not match {schema_key} schema:\n{}",
                msgs.join("\n")
            );
        }
    }

    #[test]
    fn snapshot_command_args_covers_all_snapshot_files() {
        let snapshot_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
        assert_snapshot_args_cover_disk(&snapshot_dir);
    }

    #[test]
    fn snapshot_skip_entries_have_reasons() {
        let skip: BTreeSet<_> = SNAPSHOT_JSON_SCHEMA_SKIP.iter().copied().collect();
        let reasons: BTreeSet<_> = SNAPSHOT_JSON_SCHEMA_SKIP_REASONS
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            skip, reasons,
            "SNAPSHOT_JSON_SCHEMA_SKIP must match SNAPSHOT_JSON_SCHEMA_SKIP_REASONS stems"
        );
        for (stem, reason) in SNAPSHOT_JSON_SCHEMA_SKIP_REASONS {
            assert!(
                !reason.is_empty(),
                "snapshot `{stem}` needs a non-empty skip reason"
            );
        }
    }

    #[test]
    fn schema_index_lists_every_schema() {
        let index_text = fs::read_to_string(schemas_index_path())
            .unwrap_or_else(|e| panic!("read schema index: {e}"));
        let on_disk = discover_committed_schema_documents();
        let indexed = parse_schemas_index_links(&index_text);
        assert_eq!(
            on_disk, indexed,
            "schema index must list every committed schema document exactly"
        );
        let file_count: usize = on_disk.values().map(|v| v.len()).sum();
        assert_eq!(file_count, 93, "expected 93 schema files indexed");
    }

    fn load_snapshot_json_stdout(name: &str, snapshot_dir: &Path) -> Value {
        let path = snapshot_dir.join(format!("{SNAPSHOT_FILE_PREFIX}{name}.snap"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read snapshot {}: {e}", path.display()));
        let json_body = text
            .rsplit_once("\n---\n")
            .map(|(_, body)| body)
            .unwrap_or_else(|| panic!("snapshot {name} missing insta separator"));
        let payload: Value = serde_json::from_str(json_body)
            .unwrap_or_else(|e| panic!("parse snapshot {}: {e}", path.display()));
        let stdout = payload["json"]["stdout"]
            .as_str()
            .unwrap_or_else(|| panic!("snapshot {name} missing json.stdout"));
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("parse json stdout in {name}: {e}"))
    }

    fn snapshot_validates_for_command(name: &str, args: &[&str], instance: &Value) -> bool {
        let schema_key = if instance.get("error").is_some() {
            "error".to_owned()
        } else {
            schema_command_key(args)
                .unwrap_or_else(|| panic!("no schema key for snapshot {name} args {args:?}"))
        };
        let schema_value = load_committed_schema(&schema_key);
        let compiled = match jsonschema::validator_for(&schema_value) {
            Ok(v) => v,
            Err(_) => return false,
        };
        compiled.is_valid(instance)
    }

    #[test]
    fn every_schema_has_a_validated_snapshot() {
        use std::collections::{BTreeMap, BTreeSet};

        let snapshot_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
        assert_snapshot_args_cover_disk(&snapshot_dir);

        let mut covered: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut error_kinds: BTreeSet<String> = BTreeSet::new();

        for name in discover_snapshot_stems(&snapshot_dir) {
            if name == "auth_render" {
                let path = snapshot_dir.join(format!("{SNAPSHOT_FILE_PREFIX}{name}.snap"));
                let text = fs::read_to_string(&path).unwrap();
                let payload: Value =
                    serde_json::from_str(text.rsplit_once("\n---\n").unwrap().1).unwrap();
                for (section, command, args) in [
                    ("sign_in", "auth", &["auth"][..]),
                    ("logout", "auth.logout", &["auth", "logout"][..]),
                ] {
                    let stdout = payload[section]["json"]["stdout"].as_str().unwrap();
                    let instance: Value = serde_json::from_str(stdout.trim()).unwrap();
                    let schema_key = schema_command_key(args).unwrap();
                    assert_eq!(schema_key, command);
                    let schema_value = load_committed_schema(command);
                    let compiled = jsonschema::validator_for(&schema_value).unwrap();
                    assert!(compiled.is_valid(&instance));
                    covered
                        .entry(command.to_owned())
                        .or_default()
                        .push(format!("{name}:{section}"));
                }
                continue;
            }
            if name == "update_render" {
                let instance = load_snapshot_json_stdout(&name, &snapshot_dir);
                assert!(snapshot_validates_for_command(
                    &name,
                    &["update"],
                    &instance
                ));
                covered
                    .entry("update".into())
                    .or_default()
                    .push(name.clone());
                continue;
            }
            if SNAPSHOT_JSON_SCHEMA_SKIP.contains(&name.as_str()) {
                continue;
            }
            let args = snapshot_args_lookup(&name)
                .unwrap_or_else(|| panic!("snapshot {name} missing from snapshot_command_args()"));
            let instance = load_snapshot_json_stdout(&name, &snapshot_dir);
            assert!(
                snapshot_validates_for_command(&name, args, &instance),
                "snapshot `{name}` JSON must validate against its schema"
            );
            let command = if instance.get("error").is_some() {
                "error".to_owned()
            } else {
                schema_command_key(args)
                    .unwrap_or_else(|| panic!("no schema key for snapshot {name} args {args:?}"))
            };
            covered.entry(command).or_default().push(name.clone());
            if let Some(kind) = instance.get("kind").and_then(|k| k.as_str()) {
                error_kinds.insert(kind.to_owned());
            }
        }

        for entry in schema_registry() {
            if entry.command == "error" {
                continue;
            }
            assert!(
                covered.contains_key(entry.command),
                "schema registry command `{}` has no validated output snapshot (covered: {:?})",
                entry.command,
                covered.keys().collect::<Vec<_>>()
            );
        }
        assert!(
            covered.contains_key("error"),
            "expected at least one snapshot validating against the error schema"
        );
        for kind in ["conflict", "would_overwrite", "usage", "invalid_pattern"] {
            assert!(
                error_kinds.contains(kind),
                "missing structured error snapshot for kind `{kind}`"
            );
        }
    }

    #[test]
    fn json_snapshots_match_schemas() {
        let snapshot_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
        assert_snapshot_args_cover_disk(&snapshot_dir);

        for name in discover_snapshot_stems(&snapshot_dir) {
            if SNAPSHOT_JSON_SCHEMA_SKIP.contains(&name.as_str()) {
                continue;
            }
            let args = snapshot_args_lookup(&name)
                .unwrap_or_else(|| panic!("snapshot {name} missing from snapshot_command_args()"));
            let path = snapshot_dir.join(format!("{SNAPSHOT_FILE_PREFIX}{name}.snap"));
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read snapshot {}: {e}", path.display()));
            let json_body = text
                .rsplit_once("\n---\n")
                .map(|(_, body)| body)
                .unwrap_or_else(|| panic!("snapshot {name} missing insta separator"));
            let payload: Value = serde_json::from_str(json_body)
                .unwrap_or_else(|e| panic!("parse snapshot {}: {e}", path.display()));
            let json_section = &payload["json"];
            let stdout = json_section["stdout"]
                .as_str()
                .unwrap_or_else(|| panic!("snapshot {name} missing json.stdout"));
            let instance: Value = serde_json::from_str(stdout.trim())
                .unwrap_or_else(|e| panic!("parse json stdout in {name}: {e}"));

            if instance.get("error").is_some() {
                validate_json_against_schema(&name, "error", &instance);
                continue;
            }

            let command = schema_command_key(args)
                .unwrap_or_else(|| panic!("no schema key for snapshot {name} args {args:?}"));
            validate_json_against_schema(&name, &command, &instance);
        }
    }
}
