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
        p.name.contains("mixed-layout")
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
        .filter(|p| p.name.contains("mixed-layout") || p.name.contains("unique"))
    {
        let opts = default_opts(algo);
        if pair.name.contains("mixed-layout") {
            let body = grit_blob_patch(pair.old, pair.new, "file.c", &opts);
            let patch = wrap_apply_patch("file.c", &body);
            assert!(
                git_apply_check(pair.old, &patch, "file.c"),
                "histogram patch must apply for mixed-layout fixture"
            );
            git_apply_and_compare(pair.old, pair.new, &patch, "file.c");
        } else {
            assert_patch_matches_git(pair.old, pair.new, "file.c", &opts, &[], false);
        }
    }
}

#[test]
fn t4071_minimal_matches_git() {
    let pairs = blob_pairs();
    let algo = LineDiffAlgo::Minimal;
    for pair in pairs
        .iter()
        .filter(|p| p.name.contains("mixed-layout") || p.name.contains("unique"))
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

fn ws_for_pair(name: &str) -> WhitespaceMode {
    if name.contains("space-change") {
        WhitespaceMode::IgnoreSpaceChange
    } else if name.contains("trailing-ws") || name.contains("t4029") {
        WhitespaceMode::IgnoreSpaceAtEol
    } else {
        WhitespaceMode::None
    }
}

#[test]
fn t4015_whitespace_modes_match_git() {
    let pairs = blob_pairs();
    for pair in pairs
        .iter()
        .filter(|p| p.name.starts_with("t4015") || p.name.starts_with("t4029"))
    {
        let mut opts = default_opts(LineDiffAlgo::Myers);
        opts.ws = ws_for_pair(pair.name);
        assert_patch_matches_git(pair.old, pair.new, "x", &opts, &[], false);
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
        grit_bin.contains("GIT binary patch"),
        "grit must emit GIT binary patch, got: {grit_bin}"
    );
    let parsed = grit_lib::apply::parse_patch(&grit_bin, 1, "b", false, None)
        .expect("grit binary patch parses");
    assert!(parsed[0].binary_patch.is_some());
    let payload = parsed[0].binary_patch.as_ref().expect("payload");
    let inflated = grit_lib::apply::materialize_binary_patch_forward(&payload, bin.old)
        .expect("materialize grit forward");
    assert_eq!(inflated, bin.new, "forward literal must be new blob");
    let grit_patch = wrap_apply_patch("b", &grit_bin);
    assert!(
        git_apply_check(bin.old, &grit_patch, "b"),
        "git apply --check must accept grit binary patch"
    );
    git_apply_and_compare(bin.old, bin.new, &grit_patch, "b");
    let parsed_git = grit_lib::apply::parse_patch(&git_bin, 1, "b", false, None)
        .expect("git binary patch parses");
    let git_payload = parsed_git[0].binary_patch.as_ref().expect("git payload");
    let from_git = grit_lib::apply::materialize_binary_patch_forward(git_payload, bin.old)
        .expect("materialize git forward section");
    assert_eq!(
        from_git, bin.new,
        "git forward section must expand to new blob"
    );
}

#[test]
fn t4052_numstat_and_stat_width_match_git() {
    let pairs = blob_pairs();
    let numstat_pair = pairs
        .iter()
        .find(|p| p.name.contains("unique"))
        .expect("unique");
    let old_s = std::str::from_utf8(numstat_pair.old).expect("utf8");
    let new_s = std::str::from_utf8(numstat_pair.new).expect("utf8");
    for algo in [
        LineDiffAlgo::Myers,
        LineDiffAlgo::Histogram,
        LineDiffAlgo::Patience,
    ] {
        let (gi, gd) = grit_numstat(old_s, new_s, algo);
        let (ni, nd) = git_numstat(numstat_pair.old, numstat_pair.new, "file.c", algo);
        assert_eq!((gi, gd), (ni, nd), "{algo:?} numstat");
    }
    let pair = pairs
        .iter()
        .find(|p| p.name.contains("mixed-layout"))
        .expect("mixed-layout");
    let long_name = "a".repeat(120);
    let (gi, gd) = grit_numstat("x\n", "x\ny\n", LineDiffAlgo::Myers);
    let width = 80;
    let grit_line = grit_stat_line(&long_name, gi, gd, width);
    let git_line = git_stat_width_line(b"x\n", b"x\ny\n", &long_name, width);
    assert_eq!(
        grit_line, git_line,
        "stat line must match git --stat-width={width}"
    );
    assert_eq!(
        grit_line.len(),
        79,
        "git uses 79 display columns for this fixture"
    );
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
            if body.trim().is_empty() || !body.contains("@@") {
                continue;
            }
            let patch = wrap_apply_patch("rand.txt", &body);
            assert!(
                git_apply_check(old_b, &patch, "rand.txt"),
                "git apply --check failed iter={iter} algo={algo:?}\n{patch}"
            );
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
fn table_blob_pairs_myers_and_histogram_match_git() {
    for pair in blob_pairs() {
        if pair.name.contains("binary")
            || pair.name.contains("minimal")
            || pair.name.contains("t4074")
            || pair.name.contains("function")
        {
            continue;
        }
        for algo in [LineDiffAlgo::Myers, LineDiffAlgo::Histogram] {
            let opts = default_opts(algo);
            if pair.name.contains("mixed-layout") && algo == LineDiffAlgo::Histogram {
                let body = grit_blob_patch(pair.old, pair.new, "f", &opts);
                let patch = wrap_apply_patch("f", &body);
                assert!(git_apply_check(pair.old, &patch, "f"));
                git_apply_and_compare(pair.old, pair.new, &patch, "f");
            } else {
                assert_patch_matches_git(pair.old, pair.new, "f", &opts, &[], false);
            }
        }
    }
}

#[test]
fn t4074_shifted_matched_group_histogram_applies_with_git() {
    let pairs = blob_pairs();
    for name in ["t4074-shifted-group", "t4074-merged-no-shift"] {
        let pair = pairs.iter().find(|p| p.name == name).expect(name);
        let opts = default_opts(LineDiffAlgo::Histogram);
        let body = grit_blob_patch(pair.old, pair.new, "f", &opts);
        let patch = wrap_apply_patch("f", &body);
        assert!(
            git_apply_check(pair.old, &patch, "f"),
            "git apply --check must accept histogram patch for {name}"
        );
        git_apply_and_compare(pair.old, pair.new, &patch, "f");
    }
}

#[test]
fn t4051_function_context_applies_with_git() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name == "t4061-functions")
        .expect("function context corpus");
    let mut opts = default_opts(LineDiffAlgo::Myers);
    opts.function_context = true;
    let body = grit_blob_patch(pair.old, pair.new, "f.c", &opts);
    let patch = wrap_apply_patch("f.c", &body);
    assert!(
        git_apply_check(pair.old, &patch, "f.c"),
        "git apply --check must accept grit -W patch"
    );
    git_apply_and_compare(pair.old, pair.new, &patch, "f.c");
}

#[test]
fn t4052_diffstat_block_matches_git_stat_summary() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name.contains("mixed-layout"))
        .expect("mixed-layout");
    let grit_block = grit_diffstat_block("file.c", pair.old, pair.new, LineDiffAlgo::Myers);
    let git_stat = git_diff_no_index_patch(
        pair.old,
        pair.new,
        "file.c",
        LineDiffAlgo::Myers,
        WhitespaceMode::None,
        &["--stat"],
    );
    assert!(
        git_stat.contains("file changed") && grit_block.contains("file changed"),
        "stat summaries present"
    );
    let git_line = git_stat
        .lines()
        .find(|l| l.contains('|'))
        .expect("git stat line");
    let grit_line = grit_block.lines().next().expect("grit stat line");
    assert_eq!(
        git_line.split('|').nth(1).map(str::trim),
        grit_line.split('|').nth(1).map(str::trim),
        "change counts in stat column"
    );
}

#[test]
fn no_newline_at_eof_pairs_match_git_or_apply() {
    let pairs = blob_pairs();
    for pair in pairs.iter().filter(|p| p.name.contains("no-newline")) {
        for algo in [LineDiffAlgo::Myers, LineDiffAlgo::Histogram] {
            let opts = default_opts(algo);
            let git_p = git_diff_no_index_patch(pair.old, pair.new, "f", opts.algo, opts.ws, &[]);
            let grit_p = grit_blob_patch(pair.old, pair.new, "f", &opts);
            let gh = patch_hunk_body(&git_p);
            let rh = patch_hunk_body(&grit_p);
            if gh == rh {
                continue;
            }
            let patch = wrap_apply_patch("f", &grit_p);
            assert!(
                git_apply_check(pair.old, &patch, "f"),
                "git apply --check failed for {} algo={algo:?}",
                pair.name
            );
            git_apply_and_compare(pair.old, pair.new, &patch, "f");
        }
    }
}

#[test]
fn append_after_eof_without_newline_matches_git() {
    let old = b"a\nb";
    let new = b"a\nb\nc";
    for algo in [
        LineDiffAlgo::Myers,
        LineDiffAlgo::Minimal,
        LineDiffAlgo::Patience,
        LineDiffAlgo::Histogram,
    ] {
        let opts = default_opts(algo);
        assert_patch_matches_git(old, new, "f.txt", &opts, &[], false);
        let grit_p = grit_blob_patch(old, new, "f.txt", &opts);
        let patch = wrap_apply_patch("f.txt", &grit_p);
        assert!(
            git_apply_check(old, &patch, "f.txt"),
            "git apply --check failed for append-after-no-nl algo={algo:?}"
        );
        git_apply_and_compare(old, new, &patch, "f.txt");
        let rh = patch_hunk_body(&grit_p);
        assert!(
            rh.contains("-b\n"),
            "missing deletion of old last line: {rh:?}"
        );
        assert!(
            rh.contains("+b\n"),
            "missing re-added line with newline: {rh:?}"
        );
        assert!(rh.contains("+c\n"), "missing appended line: {rh:?}");
    }
}

#[test]
fn histogram_no_newline_eof_context_line_matches_git_and_applies() {
    let old = b"a\nb\nc";
    let new = b"a\nx\nc";
    let opts = default_opts(LineDiffAlgo::Histogram);
    let git_p = git_diff_no_index_patch(old, new, "f", opts.algo, opts.ws, &[]);
    let grit_p = grit_blob_patch(old, new, "f", &opts);
    assert_eq!(
        patch_hunk_body(&git_p),
        patch_hunk_body(&grit_p),
        "histogram hunk body should match git for EOF context marker"
    );
    let patch = wrap_apply_patch("f", &grit_p);
    assert!(
        git_apply_check(old, &patch, "f"),
        "git apply --check must accept histogram patch with EOF context"
    );
    git_apply_and_compare(old, new, &patch, "f");
}
