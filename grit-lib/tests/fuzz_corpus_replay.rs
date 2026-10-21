//! Replay committed fuzz seed/crash corpora through the same entry points as `cargo fuzz`.
//!
//! Runs under stable Rust in `cargo test -p grit-lib` so parser regressions are caught without nightly.

use std::fs;
use std::path::{Path, PathBuf};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus")
}

fn replay_dir(target: &str, f: fn(&[u8])) {
    let dir = corpus_root().join(target);
    if !dir.is_dir() {
        return;
    }
    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry in {}: {e}", dir.display()));
        let ty = entry
            .file_type()
            .unwrap_or_else(|e| panic!("file type: {e}"));
        if ty.is_file() {
            let data = fs::read(entry.path())
                .unwrap_or_else(|e| panic!("read {}: {e}", entry.path().display()));
            f(&data);
        }
    }
}

macro_rules! replay_targets {
    ($($name:ident => $fn:path,)*) => {
        $(
            #[test]
            fn $name() {
                replay_dir(stringify!($name), $fn);
            }
        )*
    };
}

replay_targets! {
    tree_parse => grit_lib::fuzz::tree_parse,
    tree_lookup_scan => grit_lib::fuzz::tree_lookup_scan,
    commit_parse => grit_lib::fuzz::commit_parse,
    tag_parse => grit_lib::fuzz::tag_parse,
    index_parse => grit_lib::fuzz::index_parse,
    pkt_line_read => grit_lib::fuzz::pkt_line_read,
    protocol_v2_response => grit_lib::fuzz::protocol_v2_response,
    config_parse => grit_lib::fuzz::config_parse,
    refspec_parse => grit_lib::fuzz::refspec_parse,
    ref_name_check => grit_lib::fuzz::ref_name_check,
    transport_url_parse => grit_lib::fuzz::transport_url_parse,
    git_date_parse => grit_lib::fuzz::git_date_parse,
    pack_idx_parse => grit_lib::fuzz::pack_idx_parse,
    packed_refs_parse => grit_lib::fuzz::packed_refs_parse,
}
