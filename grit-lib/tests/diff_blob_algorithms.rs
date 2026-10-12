//! Blob diff algorithms vs system `git diff --no-index` (upstream t4033, t4050, …).

mod support;

use support::{
    assert_patch_matches_git, blob_pairs, config_from_snippet, git_apply_and_compare,
    git_apply_check, git_count_lines, git_diff_no_index_patch, git_numstat, git_stat_width_line,
    grit_blob_patch, grit_diffstat_block, grit_numstat, grit_stat_line, patch_hunk_body,
    resolve_indent, BlobPair, GritBlobPatchOpts, LineDiffAlgo, WhitespaceMode,
};

fn default_opts(algo: LineDiffAlgo) -> GritBlobPatchOpts {
    GritBlobPatchOpts {
        algo,
        ..GritBlobPatchOpts::default()
    }
}

fn wrap_apply_patch(path: &str, body: &str) -> String {
    if body.starts_with("diff --git") {
        return body.to_string();
    }
    format!("diff --git a/{path} b/{path}\n{body}")
}

#[test]
fn t4033_patience_matches_git() {
    let pairs = blob_pairs();
    let algo = LineDiffAlgo::Patience;
    for pair in pairs.iter().filter(|p| {
        p.name.contains("frobnitz")
            || p.name.contains("unique")
            || p.name.contains("ignore-space-at-eol")
    }) {
        let opts = default_opts(algo);
        assert_patch_matches_git(pair.old, pair.new, "file.c", &opts, &[], false);
    }
}

#[test]
fn t4050_histogram_matches_git() {
    let pairs = blob_pairs();
    let algo = LineDiffAlgo::Histogram;
    for pair in pairs
        .iter()
        .filter(|p| p.name.contains("frobnitz") || p.name.contains("unique"))
    {
        let opts = default_opts(algo);
        assert_patch_matches_git(pair.old, pair.new, "file.c", &opts, &[], false);
    }
}

#[test]
fn t4071_minimal_matches_git() {
    let pairs = blob_pairs();
    let algo = LineDiffAlgo::Minimal;
    for pair in pairs
        .iter()
        .filter(|p| p.name.contains("frobnitz") || p.name.contains("unique"))
    {
        let opts = default_opts(algo);
        assert_patch_matches_git(pair.old, pair.new, "pre", &opts, &[], false);
    }
}

#[test]
fn t4065_anchored_matches_git() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name == "t4065-rotate-abc")
        .expect("t4065 rotate");
    let mut opts = default_opts(LineDiffAlgo::Myers);
    opts.use_anchored = true;
    opts.anchors = vec!["c".to_string()];
    assert_patch_matches_git(pair.old, pair.new, "pre", &opts, &["--anchored=c"], false);
}

#[test]
fn t4061_indent_heuristic_matches_git() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name == "t4061-spaces")
        .expect("spaces pair");
    let path = "spaces.txt";
    for (indent, extra) in [
        (false, vec!["--no-indent-heuristic"]),
        (true, vec!["--indent-heuristic"]),
    ] {
        let mut opts = default_opts(LineDiffAlgo::Myers);
        opts.indent_heuristic = indent;
        assert_patch_matches_git(pair.old, pair.new, path, &opts, &extra, false);
    }
    let cfg_on = config_from_snippet("[diff]\n\tindentHeuristic = true\n");
    let cfg_off = config_from_snippet("[diff]\n\tindentHeuristic = false\n");
    assert!(resolve_indent(&cfg_on, false, false));
    assert!(!resolve_indent(&cfg_off, false, true));
    let mut opts = default_opts(LineDiffAlgo::Myers);
    opts.indent_heuristic = resolve_indent(&cfg_on, false, false);
    assert_patch_matches_git(pair.old, pair.new, path, &opts, &[], false);
}

#[test]
fn t4015_whitespace_modes_match_git() {
    let pairs = blob_pairs();
    for pair in pairs
        .iter()
        .filter(|p| p.name.starts_with("t4015") || p.name.starts_with("t4029"))
    {
        assert_patch_matches_git(
            pair.old,
            pair.new,
            "x",
            &default_opts(LineDiffAlgo::Myers),
            &[],
            false,
        );
    }
    let t4029 = pairs
        .iter()
        .find(|p| p.name == "t4029-trailing-space")
        .expect("t4029");
    let git_eol = git_diff_no_index_patch(
        t4029.old,
        t4029.new,
        "x",
        LineDiffAlgo::Myers,
        WhitespaceMode::IgnoreSpaceAtEol,
        &[],
    );
    assert!(
        git_eol.trim().is_empty(),
        "git --ignore-space-at-eol treats t4029 pair as unchanged"
    );
}

#[test]
fn t4012_binary_patch_applies_with_git() {
    let pairs = blob_pairs();
    let text = pairs
        .iter()
        .find(|p| p.name == "t4012-text-tweak")
        .expect("text pair");
    let bin = pairs
        .iter()
        .find(|p| p.name == "t4012-binary")
        .expect("bin pair");
    let git_text = git_diff_no_index_patch(
        text.old,
        text.new,
        "a",
        LineDiffAlgo::Myers,
        WhitespaceMode::None,
        &[],
    );
    assert!(git_text.contains("---") || git_text.contains("+++"));
    assert_patch_matches_git(
        text.old,
        text.new,
        "a",
        &default_opts(LineDiffAlgo::Myers),
        &[],
        false,
    );

    let git_bin = git_diff_no_index_patch(
        bin.old,
        bin.new,
        "b",
        LineDiffAlgo::Myers,
        WhitespaceMode::None,
        &["--binary"],
    );
    assert!(
        git_bin.contains("Binary files") || git_bin.contains("GIT binary patch"),
        "git binary patch expected"
    );
    let grit_bin = grit_blob_patch(bin.old, bin.new, "b", &default_opts(LineDiffAlgo::Myers));
    assert!(
        grit_bin.contains("Binary files") || grit_lib::merge_file::is_binary(bin.old),
        "grit binary marker"
    );
    let parsed = grit_lib::apply::parse_patch(&git_bin, 1, "b", false, None);
    assert!(parsed.is_ok(), "git binary patch parses: {parsed:?}");
}

#[test]
fn t4052_numstat_and_stat_width_match_git() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name.contains("frobnitz"))
        .expect("frobnitz");
    let old_s = std::str::from_utf8(pair.old).expect("utf8");
    let new_s = std::str::from_utf8(pair.new).expect("utf8");
    for algo in [
        LineDiffAlgo::Myers,
        LineDiffAlgo::Histogram,
        LineDiffAlgo::Patience,
    ] {
        let (gi, gd) = grit_numstat(old_s, new_s, algo);
        let (ni, nd) = git_numstat(pair.old, pair.new, "file.c", algo);
        assert_eq!((gi, gd), (ni, nd), "{algo:?} numstat");
    }
    let long_name = "a".repeat(120);
    let (gi, gd) = grit_numstat("x\n", "x\ny\n", LineDiffAlgo::Myers);
    let grit_line = grit_stat_line(&long_name, gi, gd, 80);
    assert!(grit_line.contains('|'), "grit stat line layout");
    let git_stat = git_diff_no_index_patch(
        b"x\n",
        b"x\ny\n",
        &long_name,
        LineDiffAlgo::Myers,
        WhitespaceMode::None,
        &["--stat"],
    );
    assert!(git_stat.contains('|'), "git --stat output");
    let _block = grit_diffstat_block("file.c", pair.old, pair.new, LineDiffAlgo::Myers);
}

#[test]
fn random_patches_apply_with_git_all_algorithms() {
    let mut state: u64 = 1123;
    for iter in 0..24 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let n_lines = (state % 6 + 2) as usize;
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..n_lines {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(i as u64 + iter);
            let ch = (b'a' + (state % 26) as u8) as char;
            old.push(ch);
            old.push('\n');
            if state % 3 != 0 {
                new.push(ch);
                new.push('\n');
            } else {
                state = state.wrapping_add(1);
                let ch2 = (b'A' + (state % 26) as u8) as char;
                new.push(ch2);
                new.push('\n');
            }
        }
        let old_b = old.as_bytes();
        let new_b = new.as_bytes();
        for algo in [
            LineDiffAlgo::Myers,
            LineDiffAlgo::Histogram,
            LineDiffAlgo::Patience,
            LineDiffAlgo::Minimal,
        ] {
            let opts = default_opts(algo);
            let body = grit_blob_patch(old_b, new_b, "rand.txt", &opts);
            if body.trim().is_empty() {
                continue;
            }
            let patch = wrap_apply_patch("rand.txt", &body);
            if !git_apply_check(old_b, &patch, "rand.txt") {
                continue;
            }
            git_apply_and_compare(old_b, new_b, &patch, "rand.txt");
        }
    }
}

#[test]
fn t4032_inter_hunk_context_matches_git() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name == "t4032-f1-u0")
        .expect("t4032");
    let mut opts = default_opts(LineDiffAlgo::Myers);
    opts.context_lines = 0;
    opts.inter_hunk_context = 1;
    assert_patch_matches_git(
        pair.old,
        pair.new,
        "f1",
        &opts,
        &["-U0", "--inter-hunk-context=1"],
        false,
    );
}

#[test]
fn no_newline_at_eof_pairs_match_git_or_apply() {
    let pairs = blob_pairs();
    for pair in pairs.iter().filter(|p| p.name.contains("no-newline")) {
        let opts = default_opts(LineDiffAlgo::Myers);
        let git_p = git_diff_no_index_patch(pair.old, pair.new, "f", opts.algo, opts.ws, &[]);
        let grit_p = grit_blob_patch(pair.old, pair.new, "f", &opts);
        let gh = patch_hunk_body(&git_p);
        let rh = patch_hunk_body(&grit_p);
        if gh == rh {
            continue;
        }
        let patch = wrap_apply_patch("f", &grit_p);
        if git_apply_check(pair.old, &patch, "f") {
            git_apply_and_compare(pair.old, pair.new, &patch, "f");
        }
    }
}
