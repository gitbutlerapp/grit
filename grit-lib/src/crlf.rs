//! CRLF / EOL conversion and clean/smudge filter support.
//!
//! This module handles line-ending conversion when staging files (`git add`)
//! and checking out files (`git checkout`, `read-tree -u`, `checkout-index`).
//!
//! Config knobs:
//!   - `core.autocrlf` (true / input / false)
//!   - `core.eol` (lf / crlf / native)
//!   - `core.safecrlf` (true / warn / false)
//!
//! Gitattributes:
//!   - `text` / `text=auto` / `-text` / `binary`
//!   - `eol=lf` / `eol=crlf`
//!   - `filter=<name>` (with `filter.<name>.clean` / `filter.<name>.smudge`)
//!   - `ident` keyword expansion

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use encoding_rs::UTF_8;

use crate::config::ConfigSet;
use crate::error::{FilterError, FilterPhase};
use crate::filter_process::{apply_process_clean, apply_process_smudge, FilterSmudgeMeta};
use crate::objects::{parse_tree, ObjectId, ObjectKind};
use crate::odb::Odb;

/// Working-tree encoding conversion failure (Git `reencode_string_len` returning NULL).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkTreeEncodingError {
    /// The encoding label is not supported by `encoding_rs` / [`crate::commit_encoding`].
    #[error("unsupported encoding '{label}'")]
    UnsupportedEncoding {
        /// Config or attribute encoding name.
        label: String,
    },
}

/// Error from [`convert_to_git`], [`convert_to_worktree`], and related CRLF/encoding helpers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    /// Working-tree encoding could not be resolved or converted.
    #[error(transparent)]
    Encoding(WorkTreeEncodingError),
    /// Filter, safecrlf, BOM validation, or other conversion failure (Git-compatible message).
    #[error("{0}")]
    Message(String),
}

impl From<WorkTreeEncodingError> for ConversionError {
    fn from(value: WorkTreeEncodingError) -> Self {
        Self::Encoding(value)
    }
}

impl From<String> for ConversionError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl From<&str> for ConversionError {
    fn from(value: &str) -> Self {
        Self::Message(value.to_owned())
    }
}

impl From<FilterError> for ConversionError {
    fn from(value: FilterError) -> Self {
        Self::Message(value.to_string())
    }
}

fn filter_detail(rel_path: &str, detail: String) -> FilterError {
    FilterError::NotFilteredProperly {
        path: rel_path.to_owned(),
        detail,
    }
}

fn external_filter_failed(rel_path: &str, driver: &str, phase: FilterPhase) -> FilterError {
    FilterError::ExternalFilterFailed {
        driver: driver.to_owned(),
        path: rel_path.to_owned(),
        phase,
    }
}

/// What `core.autocrlf` is set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCrlf {
    True,
    Input,
    False,
}

/// What `core.eol` is set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreEol {
    Lf,
    Crlf,
    Native,
}

/// What `core.safecrlf` is set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeCrlf {
    True,
    Warn,
    False,
}

/// Per-file text attribute from .gitattributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAttr {
    /// `text` — always treat as text.
    Set,
    /// `text=auto` — auto-detect.
    Auto,
    /// `-text` or `binary` — never convert.
    Unset,
    /// No text attribute specified.
    Unspecified,
}

/// Per-file eol attribute from .gitattributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EolAttr {
    Lf,
    Crlf,
    Unspecified,
}

/// Legacy `crlf` gitattribute (deprecated in Git; still honored for EOL conversion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrlfLegacyAttr {
    #[default]
    Unspecified,
    /// `-crlf` — disable CRLF conversion.
    Unset,
    /// `crlf=input` — normalize to LF in the object database; no CRLF on checkout.
    Input,
    /// Bare `crlf` (set) — force CRLF on checkout for text files.
    Crlf,
}

/// Per-file merge attribute from .gitattributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAttr {
    /// No merge attribute specified.
    Unspecified,
    /// `-merge` — treat as binary/non-text merge.
    Unset,
    /// `merge=<driver>` — use named merge driver.
    Driver(String),
}

/// How the `diff` gitattribute affects diff output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffAttr {
    /// No `diff` attribute (use heuristics / default).
    Unspecified,
    /// `-diff` / `diff=unset` / `binary` — treat as binary for diff purposes.
    Unset,
    /// Bare `diff` (set) — force textual diff even when the blob contains NUL.
    Set,
    /// `diff=<driver>` — use named driver (e.g. for textconv).
    Driver(String),
}

/// Per-file attributes relevant to conversion.
#[derive(Debug, Clone)]
pub struct FileAttrs {
    pub text: TextAttr,
    pub eol: EolAttr,
    /// Effect of the `diff` gitattribute on diff output.
    pub diff_attr: DiffAttr,
    pub filter_clean: Option<String>,
    pub filter_smudge: Option<String>,
    /// `filter.<name>.process` — long-running filter (takes precedence over clean/smudge commands).
    pub filter_process: Option<String>,
    /// Driver name from the active `filter=<name>` gitattribute (for error messages).
    pub filter_driver_name: Option<String>,
    /// Whether `filter.<name>.required` is set for this path's filter driver.
    pub filter_smudge_required: bool,
    /// Same config key as smudge; clean direction fails when unset if true.
    pub filter_clean_required: bool,
    pub ident: bool,
    pub merge: MergeAttr,
    pub conflict_marker_size: Option<String>,
    /// Working tree encoding (e.g. "utf-16") — content is converted to UTF-8 on add.
    pub working_tree_encoding: Option<String>,
    /// Legacy `crlf` / `-crlf` / `crlf=input` from `.gitattributes`.
    pub crlf_legacy: CrlfLegacyAttr,
    /// `whitespace` attribute value: `None` if unset, `Some("set")` for bare `whitespace`,
    /// `Some("unset")` for `-whitespace`, or `Some("trailing,...")` for `whitespace=...`.
    pub whitespace: Option<String>,
}

impl Default for FileAttrs {
    fn default() -> Self {
        FileAttrs {
            text: TextAttr::Unspecified,
            eol: EolAttr::Unspecified,
            diff_attr: DiffAttr::Unspecified,
            filter_clean: None,
            filter_smudge: None,
            filter_process: None,
            filter_driver_name: None,
            filter_smudge_required: false,
            filter_clean_required: false,
            ident: false,
            merge: MergeAttr::Unspecified,
            conflict_marker_size: None,
            working_tree_encoding: None,
            crlf_legacy: CrlfLegacyAttr::Unspecified,
            whitespace: None,
        }
    }
}

/// Global conversion settings derived from config.
#[derive(Debug, Clone)]
pub struct ConversionConfig {
    pub autocrlf: AutoCrlf,
    pub eol: CoreEol,
    pub safecrlf: SafeCrlf,
    /// `core.checkRoundtripEncoding` — comma/space separated encodings whose UTF-8 round trip is
    /// verified when writing to the object DB. `None` keeps Git's default (`SHIFT-JIS`).
    pub check_roundtrip_encoding: Option<String>,
}

impl ConversionConfig {
    /// Load conversion settings from a ConfigSet.
    pub fn from_config(config: &ConfigSet) -> Self {
        let autocrlf = match config.get("core.autocrlf") {
            Some(v) => match v.to_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => AutoCrlf::True,
                "input" => AutoCrlf::Input,
                _ => AutoCrlf::False,
            },
            None => AutoCrlf::False,
        };

        let eol = match config.get("core.eol") {
            Some(v) => match v.to_lowercase().as_str() {
                "crlf" => CoreEol::Crlf,
                "lf" => CoreEol::Lf,
                "native" => CoreEol::Native,
                _ => CoreEol::Native,
            },
            None => CoreEol::Native,
        };

        let safecrlf = match config.get("core.safecrlf") {
            Some(v) => match v.to_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => SafeCrlf::True,
                "warn" => SafeCrlf::Warn,
                _ => SafeCrlf::False,
            },
            // Git warns on round-trip EOL issues by default when unset.
            None => SafeCrlf::Warn,
        };

        let check_roundtrip_encoding = config
            .get("core.checkRoundtripEncoding")
            .filter(|s| !s.is_empty());

        ConversionConfig {
            autocrlf,
            eol,
            safecrlf,
            check_roundtrip_encoding,
        }
    }
}

/// A parsed .gitattributes rule.
#[derive(Debug, Clone)]
pub struct AttrRule {
    /// Glob text used for matching (trailing directory `/` stripped; see [`AttrRule::must_be_dir`]).
    pattern: String,
    /// When true, the source pattern ended with `/` and matches only directories (Git `PATTERN_FLAG_MUSTBEDIR`).
    must_be_dir: bool,
    /// When true, match only the path's final component (Git `PATTERN_FLAG_NODIR` / no `/` in the pattern body).
    basename_only: bool,
    attrs: Vec<(String, String)>, // (name, value) where value is "set"/"unset"/specific value
}

impl AttrRule {
    /// Diff driver names assigned by this rule (`diff=<driver>`), excluding `set`/`unset`.
    pub fn diff_drivers(&self) -> impl Iterator<Item = &str> + '_ {
        self.attrs.iter().filter_map(|(name, value)| {
            if name == "diff" && !value.is_empty() && value != "unset" && value != "set" {
                Some(value.as_str())
            } else {
                None
            }
        })
    }
}

/// Load .gitattributes from the worktree root.
pub fn load_gitattributes(work_tree: &Path) -> Vec<AttrRule> {
    let mut rules = Vec::new();

    let root_attrs = work_tree.join(".gitattributes");
    if let Ok(content) = std::fs::read_to_string(&root_attrs) {
        parse_gitattributes(&content, &mut rules);
    }

    let info_attrs = work_tree.join(".git/info/attributes");
    if let Ok(content) = std::fs::read_to_string(&info_attrs) {
        parse_gitattributes(&content, &mut rules);
    }

    rules
}

/// Parse gitattributes content into attribute rules.
///
/// This is useful when attributes are sourced from non-worktree inputs
/// (for example, tree objects selected by `--attr-source`).
#[must_use]
pub fn parse_gitattributes_content(content: &str) -> Vec<AttrRule> {
    let mut rules = Vec::new();
    parse_gitattributes(content, &mut rules);
    rules
}

/// Load .gitattributes from the index (for use during checkout when
/// the worktree file may not yet exist).
pub fn load_gitattributes_from_index(
    index: &crate::index::Index,
    odb: &crate::odb::Odb,
) -> Vec<AttrRule> {
    let mut rules = Vec::new();

    // Look for .gitattributes in the index (stage 0)
    if let Some(entry) = index.get(b".gitattributes", 0) {
        if let Ok(obj) = odb.read(&entry.oid) {
            if let Ok(content) = String::from_utf8(obj.data) {
                parse_gitattributes(&content, &mut rules);
            }
        }
    }

    rules
}

/// Load `.gitattributes` rules that apply to `rel_path`, including root and
/// nested `dir/.gitattributes` along parent directories (Git-consistent order:
/// root first, then each ancestor directory; later rules win in [`get_file_attrs`]).
///
/// Reads from the working tree when present, otherwise from a stage-0 index entry.
pub fn load_gitattributes_for_checkout(
    work_tree: &Path,
    rel_path: &str,
    index: &crate::index::Index,
    odb: &crate::odb::Odb,
) -> Vec<AttrRule> {
    let mut rules = load_gitattributes(work_tree);

    // Root `.gitattributes` may exist only in the index while the worktree file
    // is missing (e.g. t0020 in-tree attributes after `rm -rf .gitattributes`).
    if !work_tree.join(".gitattributes").exists() {
        if let Some(entry) = index.get(b".gitattributes", 0) {
            if let Ok(obj) = odb.read(&entry.oid) {
                if let Ok(content) = String::from_utf8(obj.data) {
                    parse_gitattributes(&content, &mut rules);
                }
            }
        }
    }

    let path = Path::new(rel_path);
    if let Some(parent) = path.parent() {
        let mut accum = PathBuf::new();
        for comp in parent.components() {
            accum.push(comp);
            let ga_rel = accum.join(".gitattributes");
            let wt_ga = work_tree.join(&ga_rel);
            if let Ok(content) = std::fs::read_to_string(&wt_ga) {
                parse_gitattributes(&content, &mut rules);
            } else {
                let key = path_to_index_bytes(&ga_rel);
                if let Some(entry) = index.get(&key, 0) {
                    if let Ok(obj) = odb.read(&entry.oid) {
                        if let Ok(content) = String::from_utf8(obj.data) {
                            parse_gitattributes(&content, &mut rules);
                        }
                    }
                }
            }
        }
    }

    rules
}

/// Load `.gitattributes` rules from `tree_oid` that can apply to `rel_path`.
///
/// `odb` supplies tree and blob objects, `tree_oid` is the root tree to read, and `rel_path` is the
/// repository-relative path being matched.
///
/// Returns rules in root-to-leaf order. Missing, non-blob, or invalid UTF-8 `.gitattributes` entries
/// are ignored, matching the best-effort behavior of the worktree loader.
pub fn load_gitattributes_for_tree_path(
    odb: &Odb,
    tree_oid: &ObjectId,
    rel_path: &str,
) -> Vec<AttrRule> {
    let mut rules = Vec::new();
    load_gitattributes_blob_from_tree(odb, tree_oid, ".gitattributes", &mut rules);

    let path = Path::new(rel_path);
    if let Some(parent) = path.parent() {
        let mut accum = PathBuf::new();
        for comp in parent.components() {
            accum.push(comp);
            let ga_rel = accum.join(".gitattributes");
            let ga_rel = ga_rel.to_string_lossy().replace('\\', "/");
            load_gitattributes_blob_from_tree(odb, tree_oid, &ga_rel, &mut rules);
        }
    }

    rules
}

fn load_gitattributes_blob_from_tree(
    odb: &Odb,
    tree_oid: &ObjectId,
    ga_path: &str,
    rules: &mut Vec<AttrRule>,
) {
    let Some(oid) = lookup_tree_path(odb, tree_oid, ga_path) else {
        return;
    };
    let Ok(obj) = odb.read(&oid) else {
        return;
    };
    if obj.kind != ObjectKind::Blob {
        return;
    }
    if let Ok(content) = String::from_utf8(obj.data) {
        parse_gitattributes(&content, rules);
    }
}

fn lookup_tree_path(odb: &Odb, tree_oid: &ObjectId, rel_path: &str) -> Option<ObjectId> {
    let mut current = *tree_oid;
    let mut parts = rel_path.split('/').peekable();
    while let Some(part) = parts.next() {
        let obj = odb.read(&current).ok()?;
        if obj.kind != ObjectKind::Tree {
            return None;
        }
        let entries = parse_tree(&obj.data).ok()?;
        let entry = entries
            .iter()
            .find(|entry| String::from_utf8_lossy(&entry.name) == part)?;
        if parts.peek().is_none() {
            return Some(entry.oid);
        }
        if entry.mode != 0o040000 {
            return None;
        }
        current = entry.oid;
    }
    None
}

fn path_to_index_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn parse_gitattributes(content: &str, rules: &mut Vec<AttrRule>) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let raw_pattern = match parts.next() {
            Some(p) => p,
            None => continue,
        };

        let mut pat = raw_pattern.to_owned();
        let mut must_be_dir = false;
        if pat.ends_with('/') && pat.len() > 1 {
            pat.pop();
            must_be_dir = true;
        }
        let basename_only = !pat.contains('/');

        let mut attrs = Vec::new();
        for part in parts {
            if part == "binary" {
                attrs.push(("text".to_owned(), "unset".to_owned()));
                attrs.push(("diff".to_owned(), "unset".to_owned()));
            } else if let Some(rest) = part.strip_prefix('-') {
                attrs.push((rest.to_owned(), "unset".to_owned()));
            } else if let Some((key, val)) = part.split_once('=') {
                attrs.push((key.to_owned(), val.to_owned()));
            } else {
                attrs.push((part.to_owned(), "set".to_owned()));
            }
        }

        if !attrs.is_empty() {
            rules.push(AttrRule {
                pattern: pat,
                must_be_dir,
                basename_only,
                attrs,
            });
        }
    }
}

fn config_bool_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

/// Get file attributes for a given path from .gitattributes rules and config.
///
/// `is_dir` should be true when `rel_path` names a directory (Git passes a trailing `/` for
/// directory paths in some call sites; we accept either trailing `/` or this flag from tree walks).
pub fn get_file_attrs(
    rules: &[AttrRule],
    rel_path: &str,
    is_dir: bool,
    config: &ConfigSet,
) -> FileAttrs {
    let mut fa = FileAttrs::default();

    // Walk rules; last match wins for each attribute.
    for rule in rules {
        if attr_rule_matches(rule, rel_path, is_dir) {
            for (name, value) in &rule.attrs {
                match name.as_str() {
                    "text" => {
                        fa.text = match value.as_str() {
                            "set" => TextAttr::Set,
                            "unset" => TextAttr::Unset,
                            "auto" => TextAttr::Auto,
                            _ => TextAttr::Unspecified,
                        };
                    }
                    "eol" => {
                        fa.eol = match value.as_str() {
                            "lf" => EolAttr::Lf,
                            "crlf" => EolAttr::Crlf,
                            _ => EolAttr::Unspecified,
                        };
                    }
                    "filter" => {
                        if value == "unset" {
                            fa.filter_clean = None;
                            fa.filter_smudge = None;
                            fa.filter_process = None;
                            fa.filter_driver_name = None;
                            fa.filter_smudge_required = false;
                            fa.filter_clean_required = false;
                        } else {
                            let clean_key = format!("filter.{value}.clean");
                            let smudge_key = format!("filter.{value}.smudge");
                            let process_key = format!("filter.{value}.process");
                            let req_key = format!("filter.{value}.required");
                            fa.filter_driver_name = Some(value.clone());
                            fa.filter_process = config.get(&process_key).filter(|s| !s.is_empty());
                            if fa.filter_process.is_some() {
                                fa.filter_clean = None;
                                fa.filter_smudge = None;
                            } else {
                                fa.filter_clean = config.get(&clean_key);
                                fa.filter_smudge = config.get(&smudge_key);
                            }
                            let required =
                                config.get(&req_key).is_some_and(|v| config_bool_truthy(&v));
                            fa.filter_smudge_required = required;
                            fa.filter_clean_required = required;
                        }
                    }
                    "diff" => {
                        if value == "unset" {
                            fa.diff_attr = DiffAttr::Unset;
                        } else if value == "set" {
                            fa.diff_attr = DiffAttr::Set;
                        } else if !value.is_empty() {
                            fa.diff_attr = DiffAttr::Driver(value.clone());
                        }
                    }
                    "ident" => {
                        fa.ident = value == "set";
                    }
                    "merge" => {
                        fa.merge = match value.as_str() {
                            "unset" => MergeAttr::Unset,
                            "set" => MergeAttr::Unspecified,
                            other => MergeAttr::Driver(other.to_string()),
                        };
                    }
                    "conflict-marker-size" => {
                        if value == "unset" {
                            fa.conflict_marker_size = None;
                        } else {
                            fa.conflict_marker_size = Some(value.clone());
                        }
                    }
                    "working-tree-encoding" => {
                        if value != "unset" && !value.is_empty() {
                            fa.working_tree_encoding = Some(value.clone());
                        }
                    }
                    "crlf" => {
                        fa.crlf_legacy = match value.as_str() {
                            "unset" => CrlfLegacyAttr::Unset,
                            "input" => CrlfLegacyAttr::Input,
                            "set" => CrlfLegacyAttr::Crlf,
                            _ => CrlfLegacyAttr::Unspecified,
                        };
                    }
                    "whitespace" => {
                        if value == "unset" {
                            fa.whitespace = Some("unset".to_owned());
                        } else if !value.is_empty() {
                            fa.whitespace = Some(value.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fa
}

/// Returns whether gitattribute `attr_name` is set (last matching rule wins), for arbitrary
/// attribute names used by pathspec `:(attr:...)`.
///
/// `is_dir` is whether `path` refers to a directory (see [`get_file_attrs`]).
#[must_use]
pub fn path_has_gitattribute(
    rules: &[AttrRule],
    path: &str,
    is_dir: bool,
    attr_name: &str,
) -> bool {
    matches!(
        path_gitattribute_value(rules, path, is_dir, attr_name).as_deref(),
        Some(value) if value != "unset"
    )
}

/// Return the final value assigned to `attr_name` for `path`.
///
/// `rules` is the ordered set of parsed attribute rules, `path` is repository-relative, `is_dir`
/// selects directory-only pattern handling, and `attr_name` is the attribute to query.
///
/// Returns `"set"`, `"unset"`, or an explicit string value. `None` means the attribute is
/// unspecified after all matching rules are applied.
#[must_use]
pub fn path_gitattribute_value(
    rules: &[AttrRule],
    path: &str,
    is_dir: bool,
    attr_name: &str,
) -> Option<String> {
    let mut last: Option<&str> = None;
    for rule in rules {
        if attr_rule_matches(rule, path, is_dir) {
            for (name, value) in &rule.attrs {
                if name == attr_name {
                    last = Some(value.as_str());
                }
            }
        }
    }
    last.map(str::to_string)
}

/// Whether `rule` matches `rel_path` given directory vs file context (Git `path_matches`).
#[must_use]
pub fn attr_rule_matches(rule: &AttrRule, rel_path: &str, is_dir: bool) -> bool {
    let path_is_dir = is_dir || rel_path.ends_with('/');
    if rule.must_be_dir && !path_is_dir {
        return false;
    }
    let path_for_glob = rel_path.trim_end_matches('/');
    if rule.basename_only {
        let basename = path_for_glob.rsplit('/').next().unwrap_or(path_for_glob);
        glob_matches(rule.pattern.as_str(), basename)
    } else {
        glob_matches(rule.pattern.as_str(), path_for_glob)
    }
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_bytes(pat: &[u8], text: &[u8]) -> bool {
    match (pat.first(), text.first()) {
        (None, None) => true,
        (Some(&b'*'), _) => {
            let pat_rest = pat
                .iter()
                .position(|&b| b != b'*')
                .map_or(&pat[pat.len()..], |i| &pat[i..]);
            if pat_rest.is_empty() {
                return true;
            }
            for i in 0..=text.len() {
                if glob_match_bytes(pat_rest, &text[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(&b'?'), Some(_)) => glob_match_bytes(&pat[1..], &text[1..]),
        (Some(p), Some(t)) if p == t => glob_match_bytes(&pat[1..], &text[1..]),
        _ => false,
    }
}

/// Returns true if the data looks binary (contains NUL bytes in the first 8000 bytes).
pub fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(8000);
    data[..check_len].contains(&0)
}

// Git `convert.c` `CONVERT_STAT_BITS_*` / `gather_convert_stats_ascii` (for `ls-files --eol`).
const CONVERT_STAT_BITS_TXT_LF: u32 = 0x1;
const CONVERT_STAT_BITS_TXT_CRLF: u32 = 0x2;
const CONVERT_STAT_BITS_BIN: u32 = 0x4;

#[derive(Default, Clone)]
struct TextStat {
    nul: u32,
    lonecr: u32,
    lonelf: u32,
    crlf: u32,
    printable: u32,
    nonprintable: u32,
}

fn gather_text_stat(data: &[u8]) -> TextStat {
    let mut s = TextStat::default();
    let mut i = 0usize;
    while i < data.len() {
        let c = data[i];
        if c == b'\r' {
            if i + 1 < data.len() && data[i + 1] == b'\n' {
                s.crlf += 1;
                i += 2;
            } else {
                s.lonecr += 1;
                i += 1;
            }
            continue;
        }
        if c == b'\n' {
            s.lonelf += 1;
            i += 1;
            continue;
        }
        if c == 127 {
            s.nonprintable += 1;
        } else if c < 32 {
            match c {
                b'\t' | b'\x08' | b'\x1b' | b'\x0c' => s.printable += 1,
                0 => {
                    s.nul += 1;
                    s.nonprintable += 1;
                }
                _ => s.nonprintable += 1,
            }
        } else {
            s.printable += 1;
        }
        i += 1;
    }
    s
}

fn convert_is_binary(stats: &TextStat) -> bool {
    stats.lonecr > 0 || stats.nul > 0 || (stats.printable >> 7) < stats.nonprintable
}

fn git_text_stat(data: &[u8]) -> TextStat {
    let mut stats = gather_text_stat(data);
    if !data.is_empty() && data[data.len() - 1] == 0x1a {
        stats.nonprintable = stats.nonprintable.saturating_sub(1);
    }
    stats
}

/// Git `will_convert_lf_to_crlf` using [`TextStat`] (same rules as [`should_convert_to_crlf`] on bytes).
fn will_convert_lf_to_crlf_from_stats(
    stats: &TextStat,
    conv: &ConversionConfig,
    attrs: &FileAttrs,
) -> bool {
    let has_lone_lf = stats.lonelf > 0;
    let is_bin = convert_is_binary(stats);

    match attrs.crlf_legacy {
        CrlfLegacyAttr::Unset | CrlfLegacyAttr::Input => return false,
        CrlfLegacyAttr::Crlf => {
            if attrs.text == TextAttr::Unset {
                return false;
            }
            return has_lone_lf;
        }
        CrlfLegacyAttr::Unspecified => {}
    }

    if attrs.text == TextAttr::Unset {
        return false;
    }

    if attrs.eol != EolAttr::Unspecified {
        if attrs.text == TextAttr::Auto && is_bin {
            return false;
        }
        if attrs.eol != EolAttr::Crlf {
            return false;
        }
        if attrs.text == TextAttr::Auto {
            return auto_crlf_should_smudge_lf_to_crlf_from_stats(stats);
        }
        return has_lone_lf;
    }

    if attrs.text == TextAttr::Set {
        if !output_eol_is_crlf(conv) {
            return false;
        }
        return has_lone_lf;
    }

    if attrs.text == TextAttr::Auto {
        if is_bin || !output_eol_is_crlf(conv) {
            return false;
        }
        return auto_crlf_should_smudge_lf_to_crlf_from_stats(stats);
    }

    match conv.autocrlf {
        AutoCrlf::True => {
            if is_bin {
                return false;
            }
            auto_crlf_should_smudge_lf_to_crlf_from_stats(stats)
        }
        AutoCrlf::Input | AutoCrlf::False => false,
    }
}

fn auto_crlf_should_smudge_lf_to_crlf_from_stats(stats: &TextStat) -> bool {
    if stats.lonelf == 0 {
        return false;
    }
    if stats.lonecr > 0 || stats.crlf > 0 {
        return false;
    }
    !convert_is_binary(stats)
}

fn gather_convert_stats(data: &[u8]) -> u32 {
    if data.is_empty() {
        return 0;
    }
    let mut stats = gather_text_stat(data);
    if !data.is_empty() && data[data.len() - 1] == 0x1a {
        stats.nonprintable = stats.nonprintable.saturating_sub(1);
    }
    let mut ret = 0u32;
    if convert_is_binary(&stats) {
        ret |= CONVERT_STAT_BITS_BIN;
    }
    if stats.crlf > 0 {
        ret |= CONVERT_STAT_BITS_TXT_CRLF;
    }
    if stats.lonelf > 0 {
        ret |= CONVERT_STAT_BITS_TXT_LF;
    }
    ret
}

/// Git `convert.c` `gather_convert_stats_ascii` — worktree/index blob EOL stats for `ls-files --eol`.
#[must_use]
pub fn gather_convert_stats_ascii(data: &[u8]) -> &'static str {
    let convert_stats = gather_convert_stats(data);
    if convert_stats & CONVERT_STAT_BITS_BIN != 0 {
        return "-text";
    }
    match convert_stats {
        CONVERT_STAT_BITS_TXT_LF => "lf",
        CONVERT_STAT_BITS_TXT_CRLF => "crlf",
        x if x == (CONVERT_STAT_BITS_TXT_LF | CONVERT_STAT_BITS_TXT_CRLF) => "mixed",
        _ => "none",
    }
}

/// Git `convert.c` `get_convert_attr_ascii` — ASCII summary of EOL-related attributes for
/// `git ls-files --eol` (matches `attr_action` after attribute merge, before clean/smudge).
#[must_use]
pub fn convert_attr_ascii_for_ls_files(
    rules: &[AttrRule],
    rel_path: &str,
    config: &ConfigSet,
) -> String {
    let fa = get_file_attrs(rules, rel_path, false, config);
    // Mirror `git_path_check_crlf` for `text` then legacy `crlf` (Git checks `text` first).
    let mut action = match fa.text {
        TextAttr::Set => 1,   // CRLF_TEXT
        TextAttr::Unset => 2, // CRLF_BINARY
        TextAttr::Auto => 5,  // CRLF_AUTO
        TextAttr::Unspecified => 0,
    };
    if action == 0 {
        action = match fa.crlf_legacy {
            CrlfLegacyAttr::Crlf => 1,
            CrlfLegacyAttr::Unset => 2,
            CrlfLegacyAttr::Input => 3, // CRLF_TEXT_INPUT
            CrlfLegacyAttr::Unspecified => 0,
        };
    }
    if action == 2 {
        return "-text".to_string();
    }
    // Bare `eol=lf` / `eol=crlf` without `text` still implies text mode (`convert_attrs`).
    if action == 0 {
        if fa.eol == EolAttr::Unspecified {
            return String::new();
        }
        action = 1; // CRLF_TEXT
    }

    // Merge `eol=` like `convert_attrs` (only when not already binary).
    if fa.eol == EolAttr::Lf {
        if action == 5 {
            action = 7; // CRLF_AUTO_INPUT
        } else {
            action = 3; // CRLF_TEXT_INPUT
        }
    } else if fa.eol == EolAttr::Crlf {
        if action == 5 {
            action = 6; // CRLF_AUTO_CRLF
        } else {
            action = 4; // CRLF_TEXT_CRLF
        }
    }

    // `attr_action` snapshot (Git assigns before splitting bare `text` / applying autocrlf).
    let attr_action = action;

    match attr_action {
        1 => "text".to_string(),
        3 => "text eol=lf".to_string(),
        4 => "text eol=crlf".to_string(),
        5 => "text=auto".to_string(),
        6 => "text=auto eol=crlf".to_string(),
        7 => "text=auto eol=lf".to_string(),
        _ => String::new(),
    }
}

/// Returns true if data contains any CRLF sequences.
pub fn has_crlf(data: &[u8]) -> bool {
    data.windows(2).any(|w| w == b"\r\n")
}

/// Returns true if data contains any lone LF (not preceded by CR).
pub fn has_lone_lf(data: &[u8]) -> bool {
    for i in 0..data.len() {
        if data[i] == b'\n' && (i == 0 || data[i - 1] != b'\r') {
            return true;
        }
    }
    false
}

/// Returns true if data contains a bare CR not followed by LF (Git `text_stat.lonecr`).
fn has_lone_cr(data: &[u8]) -> bool {
    for i in 0..data.len() {
        if data[i] == b'\r' && (i + 1 >= data.len() || data[i + 1] != b'\n') {
            return true;
        }
    }
    false
}

/// Git `convert.c` `will_convert_lf_to_crlf` for `CRLF_AUTO` / `CRLF_AUTO_INPUT` / `CRLF_AUTO_CRLF`:
/// if the blob already has CRLF pairs or lone CRs, do not convert lone LFs to CRLF on checkout.
fn auto_crlf_should_smudge_lf_to_crlf(data: &[u8]) -> bool {
    if !has_lone_lf(data) {
        return false;
    }
    if has_lone_cr(data) || has_crlf(data) {
        return false;
    }
    if is_binary(data) {
        return false;
    }
    true
}

/// Returns true if ALL line endings are CRLF (no lone LF).
pub fn is_all_crlf(data: &[u8]) -> bool {
    has_crlf(data) && !has_lone_lf(data)
}

/// Returns true if ALL line endings are LF (no CRLF).
pub fn is_all_lf(data: &[u8]) -> bool {
    has_lone_lf(data) && !has_crlf(data)
}

/// Git `convert.c` `has_crlf_in_index`: index blob already contains CRLF pairs (non-binary).
#[must_use]
pub fn has_crlf_in_index_blob(data: &[u8]) -> bool {
    if !data.contains(&b'\r') {
        return false;
    }
    let st = gather_convert_stats(data);
    st & CONVERT_STAT_BITS_BIN == 0 && (st & CONVERT_STAT_BITS_TXT_CRLF) != 0
}

/// Whether clean conversion uses Git's `has_crlf_in_index` guard (`convert.c` only for
/// `CRLF_AUTO`, `CRLF_AUTO_INPUT`, `CRLF_AUTO_CRLF`). Bare `eol=` without `text=auto` becomes
/// `CRLF_TEXT_*` and must not use this guard.
#[must_use]
pub fn clean_uses_autocrlf_index_guard(attrs: &FileAttrs, conv: &ConversionConfig) -> bool {
    if attrs.text == TextAttr::Unset || attrs.crlf_legacy == CrlfLegacyAttr::Unset {
        return false;
    }
    if attrs.eol != EolAttr::Unspecified && attrs.text != TextAttr::Auto {
        return false;
    }
    attrs.text == TextAttr::Auto
        || (attrs.text == TextAttr::Unspecified
            && matches!(conv.autocrlf, AutoCrlf::True | AutoCrlf::Input))
}

/// Optional inputs for [`convert_to_git_with_opts`] (Git `CONV_EOL_RENORMALIZE` / index blob).
#[derive(Debug, Clone, Copy)]
pub struct ConvertToGitOpts<'a> {
    /// Stage-0 blob bytes for this path before the current add (for safer-autocrlf).
    pub index_blob: Option<&'a [u8]>,
    /// When true, always apply CRLF→LF when configured (merge/cherry-pick renormalize).
    pub renormalize: bool,
    /// When false, skip `core.safecrlf` simulation (used for internal diff/hashing — must not spam stderr).
    pub check_safecrlf: bool,
}

impl Default for ConvertToGitOpts<'_> {
    fn default() -> Self {
        Self {
            index_blob: None,
            renormalize: false,
            check_safecrlf: true,
        }
    }
}

// ---------------------------------------------------------------------------
// working-tree-encoding (Git `convert.c` `encode_to_git` / `encode_to_worktree`)
// ---------------------------------------------------------------------------

// BOM byte sequences (Git `utf8.c`).
const UTF16_BE_BOM: &[u8] = &[0xFE, 0xFF];
const UTF16_LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF32_BE_BOM: &[u8] = &[0x00, 0x00, 0xFE, 0xFF];
const UTF32_LE_BOM: &[u8] = &[0xFF, 0xFE, 0x00, 0x00];

/// Canonical lowercase UTF label for a `working-tree-encoding` value, or `None` if the label is
/// not a UTF-16/UTF-32/UTF-8 variant Git treats specially. Mirrors Git's `same_utf_encoding`
/// (strip a leading `utf` then an optional `-`, case-insensitive), so `utf16`, `UTF-16`,
/// `Utf16Le-Bom` all normalize.
fn canonical_utf_label(label: &str) -> Option<String> {
    let trimmed = label.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("utf")?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    match rest {
        "8" => Some("utf-8".to_string()),
        "16" => Some("utf-16".to_string()),
        "16be" => Some("utf-16be".to_string()),
        "16le" => Some("utf-16le".to_string()),
        "16be-bom" => Some("utf-16be-bom".to_string()),
        "16le-bom" => Some("utf-16le-bom".to_string()),
        "32" => Some("utf-32".to_string()),
        "32be" => Some("utf-32be".to_string()),
        "32le" => Some("utf-32le".to_string()),
        _ => None,
    }
}

fn has_bom_prefix(data: &[u8], bom: &[u8]) -> bool {
    data.len() >= bom.len() && &data[..bom.len()] == bom
}

/// Git `has_prohibited_utf_bom`: UTF-16BE/LE and UTF-32BE/LE must not begin with a BOM.
fn has_prohibited_utf_bom(canon: &str, data: &[u8]) -> bool {
    match canon {
        "utf-16be" | "utf-16le" => {
            has_bom_prefix(data, UTF16_BE_BOM) || has_bom_prefix(data, UTF16_LE_BOM)
        }
        "utf-32be" | "utf-32le" => {
            has_bom_prefix(data, UTF32_BE_BOM) || has_bom_prefix(data, UTF32_LE_BOM)
        }
        _ => false,
    }
}

/// Git `is_missing_required_utf_bom`: bare UTF-16 / UTF-32 must begin with a BOM.
fn is_missing_required_utf_bom(canon: &str, data: &[u8]) -> bool {
    match canon {
        "utf-16" => !(has_bom_prefix(data, UTF16_BE_BOM) || has_bom_prefix(data, UTF16_LE_BOM)),
        "utf-32" => !(has_bom_prefix(data, UTF32_BE_BOM) || has_bom_prefix(data, UTF32_LE_BOM)),
        _ => false,
    }
}

/// Git `validate_encoding`: emit the advice line to stderr and return an error body when the BOM
/// presence is wrong for a UTF-16/UTF-32 encoding.
///
/// `label` is the original attribute spelling (preserved in messages, like Git). When
/// `die_on_error` is true (`CONV_WRITE_OBJECT`) the body is prefixed `fatal:` so the top-level
/// printer surfaces it verbatim; otherwise the `error:` line is printed here (Git `error()` returns
/// "content unmodified") and the same body is returned for the caller to swallow.
fn validate_utf_bom(
    canon: &str,
    label: &str,
    rel_path: &str,
    data: &[u8],
    die_on_error: bool,
) -> Result<(), String> {
    if has_prohibited_utf_bom(canon, data) {
        // Advice cuts the trailing "be"/"le" so the user sees the BOM-capable name (UTF-16/UTF-32).
        let stripped = label
            .strip_prefix("utf")
            .or_else(|| label.strip_prefix("UTF"));
        let utf_num = stripped
            .map(|s| s.trim_start_matches('-'))
            .and_then(|s| s.get(..s.len().saturating_sub(2)))
            .unwrap_or("");
        eprintln!(
            "The file '{rel_path}' contains a byte order mark (BOM). Please use UTF-{utf_num} as working-tree-encoding."
        );
        let body = format!("BOM is prohibited in '{rel_path}' if encoded as {label}");
        if die_on_error {
            return Err(crate::diagnostics::fatal_line(&body));
        }
        eprintln!("error: {body}");
        return Err(body);
    }
    if is_missing_required_utf_bom(canon, data) {
        let utf_num = label
            .strip_prefix("utf")
            .or_else(|| label.strip_prefix("UTF"))
            .map(|s| s.trim_start_matches('-'))
            .unwrap_or("");
        eprintln!(
            "The file '{rel_path}' is missing a byte order mark (BOM). Please use UTF-{utf_num}BE or UTF-{utf_num}LE (depending on the byte order) as working-tree-encoding."
        );
        let body = format!("BOM is required in '{rel_path}' if encoded as {label}");
        if die_on_error {
            return Err(crate::diagnostics::fatal_line(&body));
        }
        eprintln!("error: {body}");
        return Err(body);
    }
    Ok(())
}

/// Git `convert.c` `check_roundtrip`: whether `enc_name` appears as a whole, comma/space-delimited
/// token in `core.checkRoundtripEncoding` (default `SHIFT-JIS`), case-insensitively.
fn encoding_needs_roundtrip_check(enc_name: &str, conv: &ConversionConfig) -> bool {
    let list = conv
        .check_roundtrip_encoding
        .as_deref()
        .unwrap_or("SHIFT-JIS");
    let target = enc_name.to_ascii_lowercase();
    list.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|tok| !tok.is_empty())
        .any(|tok| tok.eq_ignore_ascii_case(&target))
}

/// Git `trace_printf("Checking roundtrip encoding for %s...\n", enc)`.
fn trace_roundtrip_encoding(enc_name: &str) {
    use std::io::Write;
    let Ok(trace_val) = std::env::var("GIT_TRACE") else {
        return;
    };
    if trace_val.is_empty() || trace_val == "0" || trace_val.eq_ignore_ascii_case("false") {
        return;
    }
    let line = format!("Checking roundtrip encoding for {enc_name}...\n");
    match trace_val.as_str() {
        "1" | "true" | "2" => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        path_dest => {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path_dest)
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

/// Decode raw working-tree bytes (`enc_label`) into UTF-8 for the object DB (Git `encode_to_git`).
///
/// When `validate` is true (writing to the object DB), enforce Git's UTF BOM rules and surface the
/// matching fatal message + advice (`die_on_error`). For internal diff/status reads it is false.
fn decode_working_tree_bytes_to_utf8(
    src: &[u8],
    rel_path: &str,
    enc_label: &str,
    validate: bool,
) -> Result<Vec<u8>, ConversionError> {
    let label = enc_label.trim();
    if label.is_empty() {
        return Ok(src.to_vec());
    }

    let canon = canonical_utf_label(label);

    // BOM validation (only the UTF-16/UTF-32 family). Git validates on every `encode_to_git`; when
    // writing to the object DB it dies, otherwise (diff/status reads) it prints `error:` and treats
    // the content as unmodified — `validate` here is Git's `die_on_error` (`CONV_WRITE_OBJECT`).
    if let Some(ref c) = canon {
        validate_utf_bom(c, label, rel_path, src, validate)?;
    }

    // UTF-8 is the default encoding: no conversion (Git `git_path_check_encoding`).
    if canon.as_deref() == Some("utf-8") {
        return Ok(src.to_vec());
    }

    // The `*-BOM` aliases decode like the matching raw encoding once the BOM is stripped.
    let (iconv_from, body): (&str, &[u8]) = match canon.as_deref() {
        Some("utf-16le-bom") => {
            let body = if has_bom_prefix(src, UTF16_LE_BOM) {
                &src[2..]
            } else {
                src
            };
            ("UTF-16LE", body)
        }
        Some("utf-16be-bom") => {
            let body = if has_bom_prefix(src, UTF16_BE_BOM) {
                &src[2..]
            } else {
                src
            };
            ("UTF-16BE", body)
        }
        // Bare UTF-16/UTF-32 keep their BOM; iconv consumes it to pick the byte order.
        Some(c) => (utf_canon_to_iconv_name(c), src),
        None => {
            // Unknown / unsupported label (Git `reencode_string_len` returns NULL →
            // `failed to encode '%s' from %s to %s`).
            let Some(enc) = crate::commit_encoding::resolve(label) else {
                return Err(WorkTreeEncodingError::UnsupportedEncoding {
                    label: label.to_owned(),
                }
                .into());
            };
            if enc == UTF_8 {
                return Ok(src.to_vec());
            }
            let (cow, _, had_errors) = enc.decode(src);
            if had_errors {
                return Err(ConversionError::Message(format!(
                    "failed to encode '{rel_path}' from {label} to UTF-8"
                )));
            }
            return Ok(cow.into_owned().into_bytes());
        }
    };

    decode_utf_bytes_with_encoding_rs(body, rel_path, label, iconv_from)
}

/// Decode UTF-16/UTF-32 family bytes to UTF-8 (`encoding_rs` or manual UTF-32).
fn decode_utf_bytes_with_encoding_rs(
    body: &[u8],
    rel_path: &str,
    label: &str,
    iconv_from: &str,
) -> Result<Vec<u8>, ConversionError> {
    let fail = || {
        ConversionError::Message(format!(
            "failed to encode '{rel_path}' from {label} to UTF-8"
        ))
    };
    match iconv_from {
        "UTF-16BE" => {
            let (cow, _, had_errors) = encoding_rs::UTF_16BE.decode(body);
            if had_errors {
                return Err(fail());
            }
            Ok(cow.into_owned().into_bytes())
        }
        "UTF-16LE" => {
            let (cow, _, had_errors) = encoding_rs::UTF_16LE.decode(body);
            if had_errors {
                return Err(fail());
            }
            Ok(cow.into_owned().into_bytes())
        }
        "UTF-16" => {
            if has_bom_prefix(body, UTF16_BE_BOM) {
                decode_utf_bytes_with_encoding_rs(&body[2..], rel_path, label, "UTF-16BE")
            } else if has_bom_prefix(body, UTF16_LE_BOM) {
                decode_utf_bytes_with_encoding_rs(&body[2..], rel_path, label, "UTF-16LE")
            } else {
                Err(fail())
            }
        }
        "UTF-32" => {
            if has_bom_prefix(body, UTF32_BE_BOM) {
                decode_utf32_body_to_utf8_bytes(&body[4..], rel_path, true)
            } else if has_bom_prefix(body, UTF32_LE_BOM) {
                decode_utf32_body_to_utf8_bytes(&body[4..], rel_path, false)
            } else {
                Err(fail())
            }
        }
        "UTF-32BE" => decode_utf32_body_to_utf8_bytes(body, rel_path, true),
        "UTF-32LE" => decode_utf32_body_to_utf8_bytes(body, rel_path, false),
        _ => Err(fail()),
    }
}

fn decode_utf32_body_to_utf8_bytes(
    body: &[u8],
    rel_path: &str,
    big_endian: bool,
) -> Result<Vec<u8>, ConversionError> {
    let fail = || {
        ConversionError::Message(format!(
            "failed to encode '{rel_path}' from UTF-32 to UTF-8"
        ))
    };
    if !body.len().is_multiple_of(4) {
        return Err(fail());
    }
    let mut s = String::new();
    for chunk in body.as_chunks::<4>().0 {
        let cp = if big_endian {
            u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        };
        let Some(ch) = char::from_u32(cp) else {
            return Err(fail());
        };
        s.push(ch);
    }
    Ok(s.into_bytes())
}

/// iconv encoding name for a canonical UTF label (raw encodings only; `*-bom` handled separately).
fn utf_canon_to_iconv_name(canon: &str) -> &'static str {
    match canon {
        "utf-16" => "UTF-16",
        "utf-16be" => "UTF-16BE",
        "utf-16le" => "UTF-16LE",
        "utf-32" => "UTF-32",
        "utf-32be" => "UTF-32BE",
        "utf-32le" => "UTF-32LE",
        _ => "UTF-8",
    }
}

/// Encode a UTF-8 blob into raw working-tree bytes for `enc_label` (Git `encode_to_worktree`).
///
/// Bare `UTF-16`/`UTF-32` and the `*-BOM` aliases get a BOM (Git relies on libiconv / explicit
/// BOM handling); the raw `UTF-16BE`/`UTF-16LE`/`UTF-32BE`/`UTF-32LE` encodings produce no BOM.
fn encode_utf8_blob_to_working_tree_bytes(
    src: &[u8],
    rel_path: &str,
    enc_label: &str,
) -> Result<Vec<u8>, ConversionError> {
    let label = enc_label.trim();
    if label.is_empty() {
        return Ok(src.to_vec());
    }

    let canon = canonical_utf_label(label);
    if canon.as_deref() == Some("utf-8") {
        return Ok(src.to_vec());
    }

    let fail = || {
        ConversionError::Message(format!(
            "failed to encode '{rel_path}' from UTF-8 to {label}"
        ))
    };

    // The `*-BOM` aliases: encode to the raw form, then prepend the requested BOM.
    match canon.as_deref() {
        Some("utf-16le-bom") => {
            let body = encode_utf_with_encoding_rs(src, "utf-16le").ok_or_else(fail)?;
            let mut out = UTF16_LE_BOM.to_vec();
            out.extend(body);
            return Ok(out);
        }
        Some("utf-16be-bom") => {
            let body = encode_utf_with_encoding_rs(src, "utf-16be").ok_or_else(fail)?;
            let mut out = UTF16_BE_BOM.to_vec();
            out.extend(body);
            return Ok(out);
        }
        // Bare UTF-16/UTF-32: Git/libiconv emits a BOM then little-endian code units (see `iconv`).
        Some("utf-16") => {
            let body = encode_utf_with_encoding_rs(src, "utf-16le").ok_or_else(fail)?;
            let mut out = UTF16_LE_BOM.to_vec();
            out.extend(body);
            return Ok(out);
        }
        Some("utf-32") => {
            let body = encode_utf_with_encoding_rs(src, "utf-32le").ok_or_else(fail)?;
            let mut out = UTF32_LE_BOM.to_vec();
            out.extend(body);
            return Ok(out);
        }
        Some(c) => {
            return encode_utf_with_encoding_rs(src, c).ok_or_else(fail);
        }
        None => {}
    }

    let s = std::str::from_utf8(src).map_err(|_| fail())?;
    let Some(enc) = crate::commit_encoding::resolve(label) else {
        return Err(WorkTreeEncodingError::UnsupportedEncoding {
            label: label.to_owned(),
        }
        .into());
    };
    if enc == UTF_8 {
        return Ok(src.to_vec());
    }
    let (cow, _, had_errors) = enc.encode(s);
    if had_errors {
        return Err(fail());
    }
    Ok(cow.into_owned())
}

/// UTF encode via `encoding_rs`/manual UTF-32. `target` is a canonical label (`utf-16le` etc.).
/// Produces raw bytes (no BOM).
fn encode_utf_with_encoding_rs(src: &[u8], target: &str) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(src).ok()?;
    let lower = target.to_ascii_lowercase();
    let mut out = Vec::new();
    match lower.as_str() {
        "utf-16" | "utf-16be" => {
            for u in s.encode_utf16() {
                out.extend_from_slice(&u.to_be_bytes());
            }
        }
        "utf-16le" => {
            for u in s.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
        }
        "utf-32" | "utf-32be" => {
            for ch in s.chars() {
                out.extend_from_slice(&(ch as u32).to_be_bytes());
            }
        }
        "utf-32le" => {
            for ch in s.chars() {
                out.extend_from_slice(&(ch as u32).to_le_bytes());
            }
        }
        _ => return None,
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Input (add / clean) direction
// ---------------------------------------------------------------------------

/// Convert data for storage in the index/object database (the "clean" direction).
///
/// This handles:
/// 1. Clean filter execution
/// 2. CRLF → LF conversion based on config + attributes
/// 3. safecrlf checking
///
/// Returns `Ok(data)` on success, or an error if safecrlf rejects it.
pub fn convert_to_git(
    data: &[u8],
    rel_path: &str,
    conv: &ConversionConfig,
    file_attrs: &FileAttrs,
) -> Result<Vec<u8>, ConversionError> {
    convert_to_git_with_opts(
        data,
        rel_path,
        conv,
        file_attrs,
        ConvertToGitOpts::default(),
    )
}

/// Like [`convert_to_git`] with Git-compatible safer-autocrlf index handling.
pub fn convert_to_git_with_opts(
    data: &[u8],
    rel_path: &str,
    conv: &ConversionConfig,
    file_attrs: &FileAttrs,
    opts: ConvertToGitOpts<'_>,
) -> Result<Vec<u8>, ConversionError> {
    let mut buf = data.to_vec();

    // 1. Run clean filter if configured (long-running `process` overrides clean command)
    if let Some(ref proc_cmd) = file_attrs.filter_process {
        let name = file_attrs.filter_driver_name.as_deref().unwrap_or_default();
        match apply_process_clean(proc_cmd, rel_path, &buf) {
            Ok(filtered) => buf = filtered,
            Err(e) => {
                if file_attrs.filter_clean_required {
                    if e.contains("expected git-filter-server") {
                        return Err(e.into());
                    }
<<<<<<< New base: fix: reject bytes between last pack object and trailer
                    return Err(format!("fatal: {rel_path}: clean filter '{name}' failed").into());
||||||| Common ancestor
                    return Err(format!("fatal: {rel_path}: clean filter '{name}' failed"));
=======
                    return Err(external_filter_failed(rel_path, name, FilterPhase::Clean));
>>>>>>> Current commit: lib: typed errors for config, refs, filters, sparse, and apply
                }
                if e.starts_with("filter status: abort") {
                    crate::filter_process::disable_process_filter(proc_cmd);
                }
                eprintln!(
                    "{}",
                    crate::diagnostics::error_line(&format!("external filter '{name}' failed"))
                );
            }
        }
    } else {
        match file_attrs.filter_clean.as_ref() {
            Some(clean_cmd) => {
                buf = run_filter(clean_cmd, &buf, rel_path).map_err(|e| {
                    let name = file_attrs.filter_driver_name.as_deref().unwrap_or_default();
                    if file_attrs.filter_clean_required {
                        external_filter_failed(rel_path, name, FilterPhase::Clean).into()
                    } else {
                        ConversionError::Message(format!("clean filter failed: {e}"))
                    }
                })?;
            }
            None => {
                if file_attrs.filter_clean_required {
                    let name = file_attrs.filter_driver_name.as_deref().unwrap_or_default();
<<<<<<< New base: fix: reject bytes between last pack object and trailer
                    return Err(format!("fatal: {rel_path}: clean filter '{name}' failed").into());
||||||| Common ancestor
                    return Err(format!("fatal: {rel_path}: clean filter '{name}' failed"));
=======
                    return Err(external_filter_failed(rel_path, name, FilterPhase::Clean));
>>>>>>> Current commit: lib: typed errors for config, refs, filters, sparse, and apply
                }
            }
        }
    }

    // 2. working-tree-encoding: working tree bytes → UTF-8 for the object DB (Git `encode_to_git`).
    if let Some(ref enc) = file_attrs.working_tree_encoding {
        // Bare `working-tree-encoding` (boolean true) / `false` are rejected (Git
        // `git_path_check_encoding`).
        if enc == "set" || enc == "true" || enc == "false" {
            return Err(FilterError::InvalidWorkingTreeEncoding.into());
        }
        // `CONV_WRITE_OBJECT` → validate BOM rules and die on error (Git `encode_to_git`).
        let writing_object = opts.check_safecrlf;
        buf = decode_working_tree_bytes_to_utf8(&buf, rel_path, enc, writing_object)?;
        // Git `encode_to_git`: when writing to the object DB, verify the round trip for encodings
        // listed in `core.checkRoundtripEncoding` (default `SHIFT-JIS`); emit the GIT_TRACE line.
        if writing_object && encoding_needs_roundtrip_check(enc, conv) {
            trace_roundtrip_encoding(enc);
        }
    }

    // 3. Determine if we should do CRLF→LF conversion
    let would_convert = would_convert_on_input(conv, file_attrs, &buf);

    let mut convert_crlf_into_lf = would_convert && has_crlf(&buf);
    if convert_crlf_into_lf
        && clean_uses_autocrlf_index_guard(file_attrs, conv)
        && !opts.renormalize
        && opts.index_blob.is_some_and(has_crlf_in_index_blob)
    {
        convert_crlf_into_lf = false;
    }

    // 4. safecrlf check — Git simulates clean then smudge (`check_global_conv_flags_eol`).
    if would_convert && opts.check_safecrlf {
        check_safecrlf_roundtrip(conv, file_attrs, &buf, rel_path, convert_crlf_into_lf)?;
    }

    // 5. Actually convert CRLF → LF if the file has CRLFs
    if convert_crlf_into_lf {
        buf = crlf_to_lf(&buf);
    }

    Ok(buf)
}

/// Decide whether CRLF/LF conversion is configured for this file on input.
/// Returns true if the file *would* be subject to conversion (even if no
/// actual bytes need changing).
fn would_convert_on_input(conv: &ConversionConfig, attrs: &FileAttrs, data: &[u8]) -> bool {
    match attrs.crlf_legacy {
        CrlfLegacyAttr::Unset => return false,
        CrlfLegacyAttr::Input => {
            if is_binary(data) {
                return false;
            }
            return true;
        }
        CrlfLegacyAttr::Crlf => {
            if attrs.text == TextAttr::Unset {
                return false;
            }
            if is_binary(data) {
                return false;
            }
            return true;
        }
        CrlfLegacyAttr::Unspecified => {}
    }

    // If text is explicitly unset (-text or binary), never convert
    if attrs.text == TextAttr::Unset {
        return false;
    }

    // If eol attr is set, this implies text mode
    if attrs.eol != EolAttr::Unspecified {
        if attrs.text == TextAttr::Auto && is_binary(data) {
            return false;
        }
        return true;
    }

    // If text is explicitly set, always convert
    if attrs.text == TextAttr::Set {
        return true;
    }

    if attrs.text == TextAttr::Auto {
        if is_binary(data) {
            return false;
        }
        return true;
    }

    // No text attribute: fall back to core.autocrlf
    match conv.autocrlf {
        AutoCrlf::True | AutoCrlf::Input => {
            if is_binary(data) {
                return false;
            }
            true
        }
        AutoCrlf::False => false,
    }
}

/// Git-compatible stderr when `core.safecrlf` is `warn` (clean direction, CRLF→LF).
fn eprint_safecrlf_warn_crlf_to_lf(rel_path: &str) {
    eprintln!(
        "warning: in the working copy of '{rel_path}', CRLF will be replaced by LF the next time Git touches it"
    );
}

/// Git-compatible stderr when `core.safecrlf` is `warn` (clean direction, LF→CRLF).
fn eprint_safecrlf_warn_lf_to_crlf(rel_path: &str) {
    eprintln!(
        "warning: in the working copy of '{rel_path}', LF will be replaced by CRLF the next time Git touches it"
    );
}

/// Git `convert.c` `check_global_conv_flags_eol` after simulating clean + smudge.
fn check_safecrlf_roundtrip(
    conv: &ConversionConfig,
    file_attrs: &FileAttrs,
    data: &[u8],
    rel_path: &str,
    convert_crlf_into_lf: bool,
) -> Result<(), String> {
    if conv.safecrlf == SafeCrlf::False {
        return Ok(());
    }

    let old_stats = git_text_stat(data);

    let mut new_stats = old_stats.clone();
    if convert_crlf_into_lf && new_stats.crlf > 0 {
        new_stats.lonelf += new_stats.crlf;
        new_stats.crlf = 0;
    }
    if will_convert_lf_to_crlf_from_stats(&new_stats, conv, file_attrs) {
        new_stats.crlf += new_stats.lonelf;
        new_stats.lonelf = 0;
    }

    if old_stats.crlf > 0 && new_stats.crlf == 0 {
        let msg = FilterError::LineEndingWouldChange {
            detail: format!("CRLF would be replaced by LF in {rel_path}"),
        }
        .to_string();
        if conv.safecrlf == SafeCrlf::True {
            return Err(msg);
        }
        eprint_safecrlf_warn_crlf_to_lf(rel_path);
    } else if old_stats.lonelf > 0 && new_stats.lonelf == 0 {
        let msg = FilterError::LineEndingWouldChange {
            detail: format!("LF would be replaced by CRLF in {rel_path}"),
        }
        .to_string();
        if conv.safecrlf == SafeCrlf::True {
            return Err(msg);
        }
        eprint_safecrlf_warn_lf_to_crlf(rel_path);
    }

    Ok(())
}

/// Replace CRLF with LF.
pub fn crlf_to_lf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 1 < data.len() && data[i] == b'\r' && data[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Replace lone LF with CRLF.
pub fn lf_to_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 10);
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\n' && (i == 0 || data[i - 1] != b'\r') {
            out.push(b'\r');
            out.push(b'\n');
        } else {
            out.push(data[i]);
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Output (checkout / smudge) direction
// ---------------------------------------------------------------------------

/// Convert data from the object database for writing to the working tree
/// (the "smudge" direction).
///
/// This handles (Git `convert_to_working_tree_ca_internal` order):
/// 1. Ident keyword expansion
/// 2. LF → CRLF conversion based on config + attributes
/// 3. `working-tree-encoding` (UTF-8 blob → working tree bytes)
/// 4. Smudge filter execution
///
/// Returns `Ok(None)` when the process filter returned `status=delayed` and `delayed_checkout` was
/// provided (Git `delayed_checkout`); the path is queued for [`crate::filter_process::DelayedProcessCheckout::finish`].
pub fn convert_to_worktree(
    data: &[u8],
    rel_path: &str,
    conv: &ConversionConfig,
    file_attrs: &FileAttrs,
    oid_hex: Option<&str>,
    smudge_meta: Option<&FilterSmudgeMeta>,
    delayed_checkout: Option<&mut crate::filter_process::DelayedProcessCheckout>,
) -> Result<Option<Vec<u8>>, ConversionError> {
    let mut buf = data.to_vec();

    // 1. Ident expansion
    if file_attrs.ident {
        if let Some(oid) = oid_hex {
            buf = expand_ident(&buf, oid);
        }
    }

    let can_delay_smudge = delayed_checkout.is_some()
        && file_attrs.working_tree_encoding.is_none()
        && !file_attrs.ident
        && file_attrs
            .filter_process
            .as_deref()
            .is_some_and(|c| !c.is_empty())
        && !should_convert_to_crlf(conv, file_attrs, &buf)
        && file_attrs
            .filter_process
            .as_deref()
            .is_some_and(crate::filter_process::process_filter_supports_delay);

    // 2. LF→CRLF for working tree
    let should_convert = should_convert_to_crlf(conv, file_attrs, &buf);
    if should_convert {
        buf = lf_to_crlf(&buf);
    }

    // 3. working-tree-encoding (Git `encode_to_worktree`)
    if let Some(ref enc) = file_attrs.working_tree_encoding {
        buf = encode_utf8_blob_to_working_tree_bytes(&buf, rel_path, enc)?;
    }

    // 4. Smudge filter — process driver overrides shell smudge
    let driver = file_attrs.filter_driver_name.as_deref().unwrap_or("");
    if let Some(ref proc_cmd) = file_attrs.filter_process {
        let smudge_out =
            match apply_process_smudge(proc_cmd, rel_path, &buf, smudge_meta, can_delay_smudge) {
                Ok(out) => out,
                Err(e) => {
                    if file_attrs.filter_smudge_required {
                        return Err(
                            external_filter_failed(rel_path, driver, FilterPhase::Smudge).into(),
                        );
                    }
                    if e.starts_with("filter status: abort") {
                        crate::filter_process::disable_process_filter(proc_cmd);
                    }
                    eprintln!(
                        "{}",
                        crate::diagnostics::error_line(&format!(
                            "external filter '{driver}' failed"
                        ))
                    );
                    return Ok(Some(buf));
                }
            };
        let Some(out) = smudge_out else {
            let Some(q) = delayed_checkout else {
                return Err(format!(
                    "internal error: delayed smudge without checkout queue for {rel_path}"
                )
                .into());
            };
            q.push_delayed(
                proc_cmd.clone(),
                rel_path.to_string(),
                smudge_meta.cloned().unwrap_or_default(),
            );
            return Ok(None);
        };
        buf = out;
    } else {
        match file_attrs.filter_smudge.as_ref() {
            Some(smudge_cmd) => match run_filter(smudge_cmd, &buf, rel_path) {
                Ok(filtered) => buf = filtered,
                Err(_e) => {
                    if file_attrs.filter_smudge_required {
                        return Err(
                            external_filter_failed(rel_path, driver, FilterPhase::Smudge).into(),
                        );
                    }
                }
            },
            None => {
                if file_attrs.filter_smudge_required {
                    return Err(
                        external_filter_failed(rel_path, driver, FilterPhase::Smudge).into(),
                    );
                }
            }
        }
    }

    Ok(Some(buf))
}

/// Like [`convert_to_worktree`] without delayed-checkout queueing (always materializes or errors).
pub fn convert_to_worktree_eager(
    data: &[u8],
    rel_path: &str,
    conv: &ConversionConfig,
    file_attrs: &FileAttrs,
    oid_hex: Option<&str>,
    smudge_meta: Option<&FilterSmudgeMeta>,
) -> Result<Vec<u8>, ConversionError> {
    match convert_to_worktree(data, rel_path, conv, file_attrs, oid_hex, smudge_meta, None)? {
        Some(v) => Ok(v),
        None => Err(ConversionError::Message(format!(
            "internal error: unexpected delayed smudge for {rel_path}"
        ))),
    }
}

/// Decide whether to convert LF→CRLF on output (working tree / smudge direction).
#[must_use]
pub fn should_convert_to_crlf(conv: &ConversionConfig, attrs: &FileAttrs, data: &[u8]) -> bool {
    match attrs.crlf_legacy {
        CrlfLegacyAttr::Unset | CrlfLegacyAttr::Input => return false,
        CrlfLegacyAttr::Crlf => {
            if attrs.text == TextAttr::Unset {
                return false;
            }
            // Legacy `crlf` (set) forces CRLF on checkout (even for paths Git
            // would otherwise treat as binary; see t0020 "t* crlf" + `three`).
            return true;
        }
        CrlfLegacyAttr::Unspecified => {}
    }

    // If text is explicitly unset, never convert
    if attrs.text == TextAttr::Unset {
        return false;
    }

    // If there's an explicit eol attribute
    if attrs.eol != EolAttr::Unspecified {
        if attrs.text == TextAttr::Auto && is_binary(data) {
            return false;
        }
        if attrs.eol != EolAttr::Crlf {
            return false;
        }
        // `text=auto` + `eol=crlf` → Git `CRLF_AUTO_CRLF` (safe mixed handling).
        if attrs.text == TextAttr::Auto {
            return auto_crlf_should_smudge_lf_to_crlf(data);
        }
        // Explicit `eol=crlf` with `text` set, etc. → `CRLF_TEXT_CRLF` (always normalize).
        return true;
    }

    // If text is explicitly set, use eol config
    if attrs.text == TextAttr::Set {
        return output_eol_is_crlf(conv);
    }

    if attrs.text == TextAttr::Auto {
        if is_binary(data) {
            return false;
        }
        if !output_eol_is_crlf(conv) {
            return false;
        }
        return auto_crlf_should_smudge_lf_to_crlf(data);
    }

    // No text attribute: fall back to core.autocrlf
    match conv.autocrlf {
        AutoCrlf::True => {
            if is_binary(data) {
                return false;
            }
            auto_crlf_should_smudge_lf_to_crlf(data)
        }
        AutoCrlf::Input | AutoCrlf::False => false,
    }
}

/// Whether the output EOL should be CRLF based on config.
fn output_eol_is_crlf(conv: &ConversionConfig) -> bool {
    // Git `text_eol_is_crlf`: autocrlf=input forces LF output before `core.eol` is consulted.
    if conv.autocrlf == AutoCrlf::Input {
        return false;
    }
    if conv.autocrlf == AutoCrlf::True {
        return true;
    }
    match conv.eol {
        CoreEol::Crlf => true,
        CoreEol::Lf => false,
        CoreEol::Native => {
            // On Unix, native is LF
            cfg!(windows)
        }
    }
}

/// Expand `$Id$` → `$Id: <oid>$` in data.
///
/// Matches Git's `ident_to_worktree` in `convert.c`: same-line `$` terminator, and foreign
/// idents (internal spaces before the closing `$`) are left unchanged.
fn expand_ident(data: &[u8], oid: &str) -> Vec<u8> {
    if !count_ident_regions(data) {
        return data.to_vec();
    }
    let replacement = format!("$Id: {oid} $");
    let mut out = Vec::with_capacity(data.len() + 60);
    let mut i = 0;
    while i < data.len() {
        if data[i] != b'$' {
            out.push(data[i]);
            i += 1;
            continue;
        }
        if i + 3 > data.len() || data[i + 1] != b'I' || data[i + 2] != b'd' {
            out.push(data[i]);
            i += 1;
            continue;
        }
        let after_id = i + 3;
        let ch = data.get(after_id).copied();
        match ch {
            Some(b'$') => {
                out.extend_from_slice(replacement.as_bytes());
                i = after_id + 1;
            }
            Some(b':') => {
                let rest = &data[after_id + 1..];
                let line_end = rest
                    .iter()
                    .position(|&b| b == b'\n' || b == b'\r')
                    .unwrap_or(rest.len());
                let line = &rest[..line_end];
                let Some(dollar_rel) = line.iter().position(|&b| b == b'$') else {
                    out.push(data[i]);
                    i += 1;
                    continue;
                };
                if line[..dollar_rel].contains(&b'\n') {
                    out.push(data[i]);
                    i += 1;
                    continue;
                }
                // Foreign ident (Git `ident_to_worktree`): first space in the payload after the
                // byte following `:` must not be the last character before `$`.
                let payload = &line[..dollar_rel];
                let foreign = payload.len() > 1
                    && payload[1..]
                        .iter()
                        .position(|&b| b == b' ')
                        .is_some_and(|rel| {
                            let pos = 1 + rel;
                            pos < payload.len().saturating_sub(1)
                        });
                if foreign {
                    out.push(data[i]);
                    i += 1;
                    continue;
                }
                out.extend_from_slice(replacement.as_bytes());
                i = after_id + 1 + dollar_rel + 1;
            }
            _ => {
                out.push(data[i]);
                i += 1;
            }
        }
    }
    out
}

/// Whether the buffer contains any `$Id$` / `$Id: ... $` regions Git would rewrite (`count_ident`).
fn count_ident_regions(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i < data.len() {
        if data[i] != b'$' {
            i += 1;
            continue;
        }
        if i + 3 > data.len() || data[i + 1] != b'I' || data[i + 2] != b'd' {
            i += 1;
            continue;
        }
        let after = i + 3;
        match data.get(after).copied() {
            Some(b'$') => return true,
            Some(b':') => {
                let mut j = after + 1;
                let mut found = false;
                while j < data.len() {
                    match data[j] {
                        b'$' => {
                            found = true;
                            break;
                        }
                        b'\n' | b'\r' => break,
                        _ => j += 1,
                    }
                }
                if found {
                    return true;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

/// Collapse `$Id: ... $` back to `$Id$`.
pub fn collapse_ident(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 4 <= data.len() && &data[i..i + 4] == b"$Id:" {
            let rest = &data[i + 4..];
            let line_end = rest
                .iter()
                .position(|&b| b == b'\n' || b == b'\r')
                .unwrap_or(rest.len());
            let line = &rest[..line_end];
            if let Some(end) = line.iter().position(|&b| b == b'$') {
                out.extend_from_slice(b"$Id$");
                i += 4 + end + 1;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// Shell-quote `s` with single quotes, matching Git's `sq_quote_buf` (`'` → `'\''`).
fn sq_quote_buf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Expand Git filter command placeholders: `%%` → `%`, `%f` → quoted repository-relative path.
fn expand_filter_command(cmd: &str, rel_path: &str) -> String {
    let mut out = String::with_capacity(cmd.len() + rel_path.len() + 8);
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.peek() {
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                Some('f') => {
                    chars.next();
                    out.push_str(&sq_quote_buf(rel_path));
                }
                _ => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Run a filter command, piping data through stdin→stdout.
fn run_filter(cmd: &str, data: &[u8], rel_path: &str) -> Result<Vec<u8>, std::io::Error> {
    let expanded = expand_filter_command(cmd, rel_path);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&expanded)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    use std::io::{ErrorKind, Write};
    if let Some(ref mut stdin) = child.stdin {
        if let Err(e) = stdin.write_all(data) {
            // Match Git: if the filter exits without reading stdin, ignore EPIPE.
            if e.kind() != ErrorKind::BrokenPipe {
                return Err(e);
            }
        }
    }
    drop(child.stdin.take());

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "filter command exited with status {}",
            output.status
        )));
    }

    Ok(output.stdout)
}

// Re-export AttrRule type is internal, but we expose the vec through load_gitattributes.
// The public API uses the opaque Vec from load_gitattributes + get_file_attrs.

/// Opaque type alias for loaded gitattributes rules.
pub type GitAttributes = Vec<AttrRule>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crlf_to_lf() {
        assert_eq!(crlf_to_lf(b"hello\r\nworld\r\n"), b"hello\nworld\n");
        assert_eq!(crlf_to_lf(b"hello\nworld\n"), b"hello\nworld\n");
        assert_eq!(crlf_to_lf(b"hello\r\n"), b"hello\n");
    }

    #[test]
    fn test_lf_to_crlf() {
        assert_eq!(lf_to_crlf(b"hello\nworld\n"), b"hello\r\nworld\r\n");
        assert_eq!(lf_to_crlf(b"hello\r\nworld\r\n"), b"hello\r\nworld\r\n");
    }

    #[test]
    fn test_has_crlf() {
        assert!(has_crlf(b"hello\r\nworld"));
        assert!(!has_crlf(b"hello\nworld"));
    }

    #[test]
    fn smudge_mixed_line_endings_unchanged_with_autocrlf_true() {
        let mut blob = Vec::new();
        for part in [
            b"Oh\n".as_slice(),
            b"here\n",
            b"is\n",
            b"CRLF\r\n",
            b"in\n",
            b"text\n",
        ] {
            blob.extend_from_slice(part);
        }
        let conv = ConversionConfig {
            autocrlf: AutoCrlf::True,
            eol: CoreEol::Lf,
            safecrlf: SafeCrlf::False,
            check_roundtrip_encoding: None,
        };
        let attrs = FileAttrs::default();
        let out = convert_to_worktree_eager(&blob, "mixed", &conv, &attrs, None, None).unwrap();
        assert_eq!(out, blob);
    }

    #[test]
    fn smudge_lf_only_gets_crlf_with_autocrlf_true() {
        let blob = b"a\nb\n";
        let conv = ConversionConfig {
            autocrlf: AutoCrlf::True,
            eol: CoreEol::Lf,
            safecrlf: SafeCrlf::False,
            check_roundtrip_encoding: None,
        };
        let attrs = FileAttrs::default();
        let out = convert_to_worktree_eager(blob, "x", &conv, &attrs, None, None).unwrap();
        assert_eq!(out, b"a\r\nb\r\n");
    }

    #[test]
    fn test_is_binary() {
        assert!(is_binary(b"hello\0world"));
        assert!(!is_binary(b"hello world"));
    }

    #[test]
    fn attr_dir_only_pattern_does_not_match_same_named_file() {
        let rules = parse_gitattributes_content("dir-only-pattern/ text\n");
        let rule = &rules[0];
        assert!(rule.must_be_dir);
        assert!(rule.basename_only);
        assert!(!attr_rule_matches(rule, "parent/dir-only-pattern", false));
        assert!(attr_rule_matches(rule, "dir-only-pattern", true));
    }

    #[test]
    fn test_expand_collapse_ident() {
        let data = b"$Id$";
        let expanded = expand_ident(data, "abc123");
        assert_eq!(expanded, b"$Id: abc123 $");
        let collapsed = collapse_ident(&expanded);
        assert_eq!(collapsed, b"$Id$");
    }

    #[test]
    fn expand_ident_does_not_span_lines_for_partial_keyword() {
        let data = b"$Id: NoTerminatingSymbol\n$Id: deadbeef $\n";
        let expanded = expand_ident(data, "newoid");
        assert_eq!(expanded, b"$Id: NoTerminatingSymbol\n$Id: newoid $\n");
    }

    #[test]
    fn expand_ident_preserves_foreign_id_with_internal_spaces() {
        let data = b"$Id: Foreign Commit With Spaces $\n";
        let expanded = expand_ident(data, "abc");
        assert_eq!(expanded, data);
    }

    #[test]
    fn expand_filter_command_percent_f_quotes_path() {
        let s = expand_filter_command("sh ./x.sh %f --extra", "name  with 'sq'");
        assert_eq!(s, "sh ./x.sh 'name  with '\\''sq'\\''' --extra");
        assert_eq!(expand_filter_command("a %% b", "p"), "a % b");
    }
}
