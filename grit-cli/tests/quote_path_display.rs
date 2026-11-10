//! Control characters in paths must not reach stdout raw (terminal escape injection).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn init_repo(dir: &std::path::Path) {
    assert!(Command::new(GRIT)
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("grit init")
        .success());
}

fn config_repo(dir: &std::path::Path) {
    assert!(Command::new(GRIT)
        .args(["config", "user.name", "N"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["config", "user.email", "n@x"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
}

fn assert_no_raw_esc(bytes: &[u8], context: &str) {
    assert!(!bytes.iter().any(|&b| b == 0x1b), "raw ESC in {context}");
}

fn assert_quoted_esc(text: &str, context: &str) {
    assert!(
        text.contains("\\033"),
        "expected C-style ESC quoting in {context}:\n{text}"
    );
}

fn assert_no_control_bytes(bytes: &[u8], context: &str) {
    assert_no_raw_esc(bytes, context);
    assert!(!bytes.contains(&0x07), "raw BEL in {context}");
}

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git")
            .success(),
        "git {:?} in {}",
        args,
        cwd.display()
    );
}

fn git_commit_file(repo: &std::path::Path, rel: &str, contents: &str, msg: &str) {
    std::fs::write(repo.join(rel), contents).unwrap();
    run_git(repo, &["add", rel]);
    run_git(
        repo,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            msg,
        ],
    );
}

#[test]
fn status_and_diff_quote_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());

    let evil = "evil\x1b[31mRED\x1b[0m.txt";
    let bell = "bell\x07.txt";
    let ctl = "ctl\x01char.txt";
    std::fs::File::create(dir.path().join(evil)).unwrap();
    std::fs::File::create(dir.path().join(bell)).unwrap();
    std::fs::File::create(dir.path().join(ctl)).unwrap();

    std::fs::File::create(dir.path().join("tick`name.txt")).unwrap();

    for args in [&["status"][..], &["status", "--markdown"][..]] {
        let out = Command::new(GRIT)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("grit status");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = out.stdout;
        assert_no_raw_esc(&stdout, &format!("status output for {args:?}"));
        let text = String::from_utf8_lossy(&stdout);
        assert_quoted_esc(&text, &format!("status {args:?}"));
        assert!(
            text.contains("\\a"),
            "expected BEL quoting in {args:?}:\n{text}"
        );
        assert!(
            text.contains("\\001"),
            "expected control-byte quoting in {args:?}:\n{text}"
        );
    }

    let md = Command::new(GRIT)
        .args(["status", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("grit status markdown");
    assert!(md.status.success());
    let md_out = String::from_utf8_lossy(&md.stdout);
    assert!(
        md_out.contains("``tick`name.txt``"),
        "expected doubled-backtick inline code for backtick filename:\n{md_out}"
    );
    assert!(
        !md_out.contains("tick\\`name"),
        "backslash-backtick is not valid CommonMark inline code:\n{md_out}"
    );

    let git_short = Command::new("git")
        .args(["status", "--short"])
        .current_dir(dir.path())
        .output()
        .expect("git status");
    assert!(git_short.status.success());
    let git_line = String::from_utf8_lossy(&git_short.stdout);
    assert!(git_line.contains("\\033"));

    assert!(Command::new(GRIT)
        .args(["add", "."])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "funny names"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    std::fs::write(dir.path().join(evil), b"touch\n").unwrap();
    let diff_out = Command::new(GRIT)
        .args(["diff", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("grit diff");
    assert!(diff_out.status.success());
    assert_no_raw_esc(&diff_out.stdout, "diff output");
}

#[test]
fn porcelain_commands_quote_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    config_repo(dir.path());

    std::fs::write(dir.path().join("base"), "base\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "base"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    let evil = "evil\x1b[31mRED\x1b[0m";
    let newline = "new\nline";
    std::fs::File::create(dir.path().join(evil)).unwrap();
    std::fs::File::create(dir.path().join(newline)).unwrap();

    let add_out = Command::new(GRIT)
        .args(["add"])
        .current_dir(dir.path())
        .output()
        .expect("grit add");
    assert!(add_out.status.success());
    assert_no_raw_esc(&add_out.stdout, "grit add stdout");
    let add_text = String::from_utf8_lossy(&add_out.stdout);
    assert_quoted_esc(&add_text, "grit add");
    assert!(
        add_text.contains("\"new\\nline\"") || add_text.contains("new\\nline"),
        "expected newline quoting in add:\n{add_text}"
    );

    let mv_out = Command::new(GRIT)
        .args(["mv", newline, "ok"])
        .current_dir(dir.path())
        .output()
        .expect("grit mv");
    assert!(mv_out.status.success());
    assert_no_control_bytes(&mv_out.stdout, "grit mv stdout");
    let mv_text = String::from_utf8_lossy(&mv_out.stdout);
    assert!(
        mv_text.contains("new\\nline"),
        "expected newline quoting in mv:\n{mv_text}"
    );

    let bell = "u\x07";
    std::fs::File::create(dir.path().join(bell)).unwrap();

    for args in [&["clean"][..], &["clean", "-f"][..]] {
        let out = Command::new(GRIT)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("grit clean");
        assert!(out.status.success());
        assert_no_raw_esc(&out.stdout, &format!("grit clean {args:?}"));
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("remove") || text.contains("Remove") {
            assert!(
                text.contains("\\033") || text.contains("\\007") || text.contains("\\a"),
                "expected quoting in clean output:\n{text}"
            );
        }
    }

    assert!(Command::new(GRIT)
        .args(["add", evil, "ok"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "stage evil"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    std::fs::write(dir.path().join(evil), b"x\n").unwrap();
    let restore_out = Command::new(GRIT)
        .args(["restore", evil])
        .current_dir(dir.path())
        .output()
        .expect("grit restore");
    assert!(restore_out.status.success());
    assert_no_raw_esc(&restore_out.stdout, "grit restore");
    assert_quoted_esc(
        &String::from_utf8_lossy(&restore_out.stdout),
        "grit restore",
    );

    std::fs::write(dir.path().join(evil), b"y\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["add", evil])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let restore_staged = Command::new(GRIT)
        .args(["restore", "--staged", evil])
        .current_dir(dir.path())
        .output()
        .expect("grit restore --staged");
    assert!(restore_staged.status.success());
    assert_no_raw_esc(&restore_staged.stdout, "grit restore --staged");

    std::fs::File::create(dir.path().join(bell)).unwrap();
    assert!(Command::new(GRIT)
        .args(["add", "-f", bell])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let rm_out = Command::new(GRIT)
        .args(["rm", "-f", bell])
        .current_dir(dir.path())
        .output()
        .expect("grit rm");
    assert!(rm_out.status.success());
    assert_no_control_bytes(&rm_out.stdout, "grit rm");
    let rm_text = String::from_utf8_lossy(&rm_out.stdout);
    assert!(
        rm_text.contains("\\a") || rm_text.contains("\\007"),
        "expected BEL quoting in rm:\n{rm_text}"
    );

    std::fs::File::create(dir.path().join(bell)).unwrap();
    assert!(Command::new(GRIT)
        .args(["add", "-f", bell])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let rm_cached = Command::new(GRIT)
        .args(["rm", "--cached", "-f", bell])
        .current_dir(dir.path())
        .output()
        .expect("grit rm --cached");
    assert!(rm_cached.status.success());
    assert_no_raw_esc(&rm_cached.stdout, "grit rm --cached");
}

#[test]
fn merge_conflict_lines_quote_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    config_repo(dir.path());

    let evil = "e\x1b[31mV\nx";
    std::fs::write(dir.path().join(evil), "start\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-am", "start"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    assert!(Command::new(GRIT)
        .args(["branch", "b"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    assert!(Command::new(GRIT)
        .args(["switch", "b"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.path().join(evil), "on b\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-am", "on b"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    assert!(Command::new(GRIT)
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.path().join(evil), "on main\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-am", "on main"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    let merge_out = Command::new(GRIT)
        .args(["merge", "b"])
        .current_dir(dir.path())
        .output()
        .expect("grit merge b");
    assert!(
        !merge_out.status.success(),
        "merge should stop on conflicts"
    );
    let combined = [merge_out.stdout.as_slice(), merge_out.stderr.as_slice()].concat();
    assert_no_control_bytes(&combined, "grit merge conflict output");
    let text = String::from_utf8_lossy(&combined);
    assert_quoted_esc(&text, "grit merge CONFLICT line");
    assert!(
        text.contains("\\n"),
        "expected newline quoting in merge conflict path:\n{text}"
    );
}

#[test]
fn add_embedded_gitlink_warning_quotes_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    config_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "init"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    let nested = "nested\x1b[31mRED\x1b[0m";
    let nested_path = dir.path().join(nested);
    std::fs::create_dir_all(&nested_path).unwrap();
    run_git(&nested_path, &["init", "-q", "-b", "main"]);
    run_git(&nested_path, &["config", "user.email", "t@example.com"]);
    run_git(&nested_path, &["config", "user.name", "T"]);
    std::fs::write(nested_path.join("inside.txt"), b"inside\n").unwrap();
    run_git(&nested_path, &["add", "inside.txt"]);
    run_git(&nested_path, &["commit", "-qm", "nested"]);

    let out = Command::new(GRIT)
        .args(["add", "."])
        .current_dir(dir.path())
        .output()
        .expect("grit add nested");
    assert!(out.status.success());
    assert_no_control_bytes(&out.stdout, "grit add stdout");
    assert_no_control_bytes(&out.stderr, "grit add gitlink stderr");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_quoted_esc(&stderr, "gitlink warning");
}

#[test]
fn stash_show_quotes_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    config_repo(dir.path());

    std::fs::write(dir.path().join("a"), "a\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "a"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    let esc = "esc\x1b[31mred.txt";
    let newline = "new\nline.txt";
    let tab = "tab\there.txt";
    std::fs::write(dir.path().join(esc), "x").unwrap();
    std::fs::write(dir.path().join(newline), "x").unwrap();
    std::fs::write(dir.path().join(tab), "x").unwrap();

    assert!(Command::new(GRIT)
        .args(["add"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["stash"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    for args in [&["stash", "show"][..], &["stash", "show", "--markdown"][..]] {
        let out = Command::new(GRIT)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("grit stash show");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_no_raw_esc(&out.stdout, &format!("stash show {args:?}"));
        let text = String::from_utf8_lossy(&out.stdout);
        assert_quoted_esc(&text, &format!("stash show {args:?}"));
        assert!(
            text.contains("\\n") || text.contains("new\\nline"),
            "expected newline quoting in {args:?}:\n{text}"
        );
        assert!(
            text.contains("\\t") || text.contains("tab\\there"),
            "expected tab quoting in {args:?}:\n{text}"
        );
        assert!(
            text.lines().filter(|l| l.contains("line.txt")).count() <= 1,
            "newline in path must not split diffstat row in {args:?}:\n{text}"
        );
    }

    let json = Command::new(GRIT)
        .args(["stash", "show", "--json"])
        .current_dir(dir.path())
        .output()
        .expect("stash show json");
    assert!(json.status.success());
    assert_no_raw_esc(&json.stdout, "stash show json");
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json");
    let paths: Vec<&str> = v["stat"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert!(
        paths.iter().any(|p| p.contains('\x1b')),
        "json keeps raw esc"
    );
    assert!(paths.iter().any(|p| p.contains('\n')), "json keeps raw nl");
    assert!(paths.iter().any(|p| p.contains('\t')), "json keeps raw tab");
    assert!(
        !paths.iter().any(|p| p.contains("\\033")),
        "json paths must not be C-quoted: {paths:?}"
    );
}

#[test]
fn pull_markdown_merge_conflict_quotes_control_bytes_in_paths() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = root.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    run_git(&upstream, &["init", "-q", "-b", "main"]);
    let evil = "pull\x1b[31mRED\x1b[0m\nline";
    git_commit_file(&upstream, evil, "one\n", "seed");
    run_git(
        root.path(),
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.path().join("client");
    config_repo(&client);
    git_commit_file(&upstream, evil, "two\n", "remote");
    git_commit_file(&client, evil, "local\n", "local");
    run_git(&client, &["fetch", "-q"]);

    let out = Command::new(GRIT)
        .args(["--markdown", "pull"])
        .current_dir(&client)
        .output()
        .expect("grit pull markdown");
    assert!(!out.status.success(), "pull should stop on merge conflicts");
    let combined = [out.stdout.as_slice(), out.stderr.as_slice()].concat();
    assert_no_control_bytes(&combined, "grit pull --markdown conflict output");
    let text = String::from_utf8_lossy(&combined);
    assert_quoted_esc(&text, "pull markdown conflict");
    assert!(
        text.contains("\\n"),
        "expected newline quoting in pull markdown conflict:\n{text}"
    );
}

const BACKTICK_FILE: &str = "a`b.txt";
const BACKTICK_INLINE: &str = "``a`b.txt``";
const BACKTICK_SPEC_INLINE: &str = "``HEAD:a`b.txt``";

fn assert_no_broken_backtick_path_span(text: &str, context: &str) {
    assert!(
        !text.contains("- `a`b.txt`"),
        "broken single-backtick name-only span in {context}:\n{text}"
    );
    assert!(
        !text.contains("- `A\ta`b.txt`"),
        "broken single-backtick name-status span in {context}:\n{text}"
    );
}

#[test]
fn markdown_inline_code_escapes_backticks_in_paths_and_specs() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    config_repo(dir.path());
    git_commit_file(dir.path(), BACKTICK_FILE, "a\n", "x");

    let spec = format!("HEAD:{BACKTICK_FILE}");

    for args in [
        &["diff", "HEAD", "--name-only", "--markdown"][..],
        &["log", "--name-only", "--markdown", "-1"][..],
        &["log", "--name-status", "--markdown", "-1"][..],
    ] {
        let out = Command::new(GRIT)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("grit");
        assert!(
            out.status.success(),
            "grit {args:?} stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        let has_doubled_backtick =
            text.contains(BACKTICK_INLINE) || text.contains("``A\ta`b.txt``");
        assert!(
            has_doubled_backtick,
            "expected doubled-backtick path in grit {args:?}:\n{text}"
        );
        assert_no_broken_backtick_path_span(&text, &format!("grit {args:?}"));
    }

    let blame = Command::new(GRIT)
        .args(["blame", BACKTICK_FILE, "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("grit blame");
    assert!(blame.status.success());
    let blame_text = String::from_utf8_lossy(&blame.stdout);
    assert!(
        blame_text.contains(&format!("## Blame: {BACKTICK_INLINE}")),
        "blame heading:\n{blame_text}"
    );

    let cat = Command::new(GRIT)
        .args(["cat", &spec, "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("grit cat");
    assert!(cat.status.success());
    let cat_text = String::from_utf8_lossy(&cat.stdout);
    assert!(
        cat_text.contains(&format!("**spec:** {BACKTICK_SPEC_INLINE}")),
        "cat spec:\n{cat_text}"
    );

    let resolve = Command::new(GRIT)
        .args(["resolve", &spec, "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("grit resolve");
    assert!(resolve.status.success());
    let resolve_text = String::from_utf8_lossy(&resolve.stdout);
    assert!(
        resolve_text.contains(BACKTICK_SPEC_INLINE),
        "resolve spec cell:\n{resolve_text}"
    );

    let show = Command::new(GRIT)
        .args(["show", &spec, "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("grit show");
    assert!(show.status.success());
    let show_text = String::from_utf8_lossy(&show.stdout);
    assert!(
        show_text.contains(&format!("## {BACKTICK_SPEC_INLINE}")),
        "show path heading:\n{show_text}"
    );
}
