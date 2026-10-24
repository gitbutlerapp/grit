//! Hidden entry points shared by `cargo fuzz` targets and [`fuzz_corpus_replay`](../../tests/fuzz_corpus_replay.rs).
//!
//! Each function must not panic on arbitrary input; callers treat errors as expected.
//!
//! ```rust
//! grit_lib::fuzz::tree_parse(b"");
//! ```

use std::io::Cursor;
use std::path::Path;

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::config::{ConfigFile, ConfigScope};
use crate::fetch::{
    parse_v2_ls_refs_response, read_v2_fetch_pack_response, NoProgress, ShallowUpdate,
};
use crate::index::Index;
use crate::objects::pack::parse_pack_index_bytes;
use crate::objects::{
    parse_commit, parse_tag, parse_tree, serialize_commit, serialize_tag, serialize_tree,
};
use crate::pkt_line;
use crate::refs::parse_packed_refs_to_map;
use crate::refspec::parse_fetch_refspec;
use url::Url;

const FUZZ_NOW_SEC: i64 = 1_700_000_000;

/// [`parse_tree`] and a serialize → parse round-trip when the first parse succeeds.
pub fn tree_parse(data: &[u8]) {
    if let Ok(entries) = parse_tree(data) {
        if let Ok(round) = parse_tree(&serialize_tree(&entries)) {
            debug_assert_eq!(entries, round);
        }
    } else {
        let _ = parse_tree(data);
    }
}

/// Walk tree bytes via span-based lookup (covers zero-copy tree paths without building a full vec).
pub fn tree_lookup_scan(data: &[u8]) {
    let _ = crate::objects::find_tree_entry_by_name_in_data(data, 20, b"", false);
    let _ = crate::objects::find_tree_entry_by_name_in_data(data, 32, b"", false);
}

/// [`parse_commit`] with serialize → parse when successful.
pub fn commit_parse(data: &[u8]) {
    if let Ok(commit) = parse_commit(data) {
        if let Ok(round) = parse_commit(&serialize_commit(&commit)) {
            debug_assert_eq!(commit.tree, round.tree);
            debug_assert_eq!(commit.parents, round.parents);
            debug_assert_eq!(commit.message, round.message);
        }
    } else {
        let _ = parse_commit(data);
    }
}

/// [`parse_tag`] with serialize → parse when successful.
pub fn tag_parse(data: &[u8]) {
    if let Ok(tag) = parse_tag(data) {
        if let Ok(round) = parse_tag(&serialize_tag(&tag)) {
            debug_assert_eq!(tag.object, round.object);
            debug_assert_eq!(tag.object_type, round.object_type);
            debug_assert_eq!(tag.tag, round.tag);
            debug_assert_eq!(tag.message, round.message);
        }
    } else {
        let _ = parse_tag(data);
    }
}

/// Index v2+ on-disk format parse (`Index::parse`).
pub fn index_parse(data: &[u8]) {
    let _ = Index::parse(data);
}

/// Pkt-line reader over arbitrary bytes (wire framing).
pub fn pkt_line_read(data: &[u8]) {
    let mut cur = Cursor::new(data);
    loop {
        match pkt_line::read_packet(&mut cur) {
            Ok(None) | Err(_) => break,
            Ok(Some(_)) => {}
        }
    }
    let mut cur = Cursor::new(data);
    let _ = pkt_line::read_until_flush_or_delim(&mut cur);
}

/// Protocol v2 response parsers (ls-refs and fetch pack demux).
pub fn protocol_v2_response(data: &[u8]) {
    let mut ls = Cursor::new(data);
    let _ = parse_v2_ls_refs_response(&mut ls);
    let mut fetch = Cursor::new(data);
    let mut pack = Vec::new();
    let mut shallow = ShallowUpdate::default();
    let mut progress = NoProgress;
    let _ = read_v2_fetch_pack_response(
        &mut fetch,
        &mut pack,
        None,
        &mut shallow,
        &mut progress,
        &crate::progress::NeverCancel,
        false,
    );
    if let Ok(s) = std::str::from_utf8(data) {
        for line in s.lines() {
            let _ = crate::protocol_v2::fetch_features(&[line.to_owned()]);
            let _ = crate::protocol_v2::fetch_prelude_shallow_before_wanted_refs(
                &line.split_whitespace().collect::<Vec<_>>(),
            );
        }
    }
}

/// Git config file parse (`ConfigFile::parse`).
pub fn config_parse(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let _ = ConfigFile::parse(Path::new("fuzz.config"), &text, ConfigScope::Local);
}

/// Fetch refspec parse.
pub fn refspec_parse(data: &[u8]) {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = parse_fetch_refspec(s.trim_end_matches(['\r', '\n']));
    }
}

/// Refname format check (lossy UTF-8 input).
pub fn ref_name_check(data: &[u8]) {
    let name = String::from_utf8_lossy(data)
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    let opts = RefNameOptions::default();
    let _ = check_refname_format(&name, &opts);
}

/// Transport URL / local path helpers.
pub fn transport_url_parse(data: &[u8]) {
    if let Ok(s) = std::str::from_utf8(data) {
        let trimmed = s.trim_end_matches(['\r', '\n']).trim();
        let _ = Url::parse(trimmed);
        let _ = crate::transport_path::url_is_local_not_ssh(trimmed);
        let _ = crate::transport_path::file_url_to_local_path(trimmed);
        let _ = crate::transport_path::looks_like_command_line_option(trimmed);
    }
}

/// Git approxidate / cutoff parsing.
pub fn git_date_parse(data: &[u8]) {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = crate::git_date::parse_date_cutoff(s.trim_end_matches(['\r', '\n']), FUZZ_NOW_SEC);
    }
}

/// Pack `.idx` open from bytes (`parse_pack_index_bytes`, verify off for fuzz throughput).
pub fn pack_idx_parse(data: &[u8]) {
    let _ = parse_pack_index_bytes(Path::new("fuzz.idx"), data.to_vec(), false);
}

/// `packed-refs` file parse.
pub fn packed_refs_parse(data: &[u8]) {
    let _ = parse_packed_refs_to_map(Path::new("fuzz/packed-refs"), data);
}
