//! A server whose large blobs were offloaded to a large-object promisor with
//! `grit gc offload`, cloned by real Git through `grit upload-pack`: accepting
//! clients fetch the blobs from the LOP, others are refused with a hint or served
//! after the server fetched the blobs itself. Needs Git 2.49 or newer (the
//! `promisor-remote` capability); older `git` prints SKIP.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use grit_test_support::git_at_least;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

/// 20 000 bytes that do not compress or delta against anything else.
fn big_content(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..20_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn isolate<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    cmd.env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_NO_LAZY_FETCH")
        .env_remove("GRIT_NO_LAZY_FETCH")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("TZ", "UTC")
}

fn run(program: &str, dir: &Path, home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(program);
    isolate(&mut cmd, home).current_dir(dir).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("{program} {args:?}: {e}"))
}

fn ok(out: Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what} failed\n--- stdout\n{}\n--- stderr\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `main.git` (offloaded with `grit gc offload`, advertising `lop`) and `lop.git`.
struct Server {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    main: PathBuf,
    lop: PathBuf,
    big: String,
}

impl Server {
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        ok(
            run("git", dir, &self.base, args, &[]),
            &format!("git {args:?}"),
        )
    }

    fn grit(&self, dir: &Path, args: &[&str]) -> String {
        ok(
            run(GRIT, dir, &self.base, args, &[]),
            &format!("grit {args:?}"),
        )
    }

    fn missing_in_main(&self) -> Vec<String> {
        self.git(
            &self.main,
            &["rev-list", "--objects", "--all", "--missing=print"],
        )
        .lines()
        .filter_map(|l| l.strip_prefix('?').map(str::to_owned))
        .collect()
    }

    fn lop_url(&self) -> String {
        format!("file://{}", self.lop.display())
    }

    /// `git clone` of main through `grit upload-pack`; `extra` goes before the URL.
    fn git_clone(&self, dest: &str, extra: &[&str], env: &[(&str, &str)]) -> Output {
        let upload_pack = format!("{GRIT} upload-pack");
        let url = format!("file://{}", self.main.display());
        let mut args = vec![
            "-c",
            "protocol.version=2",
            "clone",
            "--no-local",
            "--upload-pack",
            upload_pack.as_str(),
        ];
        args.extend_from_slice(extra);
        args.extend([url.as_str(), dest]);
        run("git", &self.base, &self.base, &args, env)
    }

    /// Clone options that accept the advertised LOP and fetch from it with `lop_upload_pack`.
    fn accepting<'a>(&'a self, lop_url: &'a str, lop_upload_pack: Option<&'a str>) -> Vec<String> {
        let mut args = vec![
            "-c".to_owned(),
            "remote.lop.promisor=true".to_owned(),
            "-c".to_owned(),
            format!("remote.lop.url={lop_url}"),
            "-c".to_owned(),
            "promisor.acceptfromserver=All".to_owned(),
            "--filter=blob:limit=5k".to_owned(),
        ];
        if let Some(up) = lop_upload_pack {
            args.extend(["-c".to_owned(), format!("remote.lop.uploadpack={up}")]);
        }
        args
    }
}

fn server() -> Option<Server> {
    if !git_at_least(2, 49) {
        eprintln!("SKIP: git older than 2.49 has no promisor-remote capability");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let work = base.join("work");
    std::fs::create_dir(&work).unwrap();
    let run_git = |dir: &Path, args: &[&str]| ok(run("git", dir, &base, args, &[]), "git setup");
    run_git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("small"), "small\n").unwrap();
    std::fs::write(work.join("big"), big_content(1)).unwrap();
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-qm", "one"]);
    run_git(
        &base,
        &["clone", "-q", "--bare", "--no-local", "work", "main.git"],
    );
    run_git(&base, &["init", "-q", "--bare", "lop.git"]);
    let main = base.join("main.git");
    let lop = base.join("lop.git");
    let big = run_git(&work, &["rev-parse", "HEAD:big"]).trim().to_owned();
    let server = Server {
        _tmp: tmp,
        base: base.clone(),
        main: main.clone(),
        lop: lop.clone(),
        big,
    };
    let lop_url = server.lop_url();
    server.git(&main, &["remote", "add", "lop", &lop_url]);
    for (k, v) in [
        ("promisor.advertise", "true"),
        ("uploadpack.allowFilter", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
    ] {
        server.git(&main, &["config", k, v]);
    }
    server.grit(
        &main,
        &[
            "gc",
            "offload",
            "--to",
            "lop",
            "--blobs",
            "5k",
            "--configure-lop",
        ],
    );
    assert_eq!(server.missing_in_main(), [server.big.clone()]);
    Some(server)
}

#[test]
fn offloaded_repository_passes_git_fsck_and_keeps_its_promise() {
    let Some(s) = server() else { return };
    s.git(&s.main, &["fsck"]);
    s.git(&s.lop, &["cat-file", "-e", &s.big]);
    let kept: Vec<_> = std::fs::read_dir(s.lop.join("objects/pack"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "keep"))
        .collect();
    assert_eq!(kept.len(), 1, "LOP pack is protected from gc");
    // Git's own gc keeps the blob promised (and does not try to fetch it).
    s.git(&s.main, &["gc", "--quiet"]);
    assert_eq!(s.missing_in_main(), [s.big.clone()]);
    s.git(&s.main, &["fsck"]);
    // So does grit's.
    s.grit(&s.main, &["gc"]);
    s.grit(&s.main, &["fsck"]);
    assert_eq!(s.missing_in_main(), [s.big.clone()]);
    // A second offload has nothing left to do.
    let again = s.grit(&s.main, &["gc", "offload", "--to", "lop", "--json"]);
    assert!(again.contains("\"candidates\":0"), "{again}");
}

#[test]
fn accepting_git_client_gets_the_blob_from_a_grit_served_lop() {
    let Some(s) = server() else { return };
    let lop_up = format!("{GRIT} upload-pack");
    let lop_url = s.lop_url();
    let extra = s.accepting(&lop_url, Some(&lop_up));
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    ok(s.git_clone("client", &extra, &[]), "accepting clone");
    let client = s.base.join("client");
    assert_eq!(std::fs::read(client.join("big")).unwrap(), big_content(1));
    s.git(&client, &["fsck"]);
    assert_eq!(s.missing_in_main(), [s.big.clone()], "main still lacks it");
}

#[test]
fn accepting_git_client_with_a_git_served_lop() {
    let Some(s) = server() else { return };
    let lop_url = s.lop_url();
    let extra = s.accepting(&lop_url, None);
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    ok(
        s.git_clone("client", &extra, &[]),
        "clone with git-served LOP",
    );
    assert_eq!(
        std::fs::read(s.base.join("client/big")).unwrap(),
        big_content(1)
    );
}

#[test]
fn non_accepting_git_client_is_refused_with_a_hint() {
    let Some(s) = server() else { return };
    let out = s.git_clone("client", &[], &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("remote error"), "{stderr}");
    assert!(stderr.contains("promisor.acceptFromServer"), "{stderr}");
    assert!(stderr.contains(&s.big), "{stderr}");
    assert_eq!(s.missing_in_main(), [s.big.clone()]);
}

#[test]
fn server_lazy_fetch_serves_clients_that_accept_nothing() {
    let Some(s) = server() else { return };
    // GIT_NO_LAZY_FETCH=0 reaches `grit upload-pack` through the environment.
    ok(
        s.git_clone("by-env", &[], &[("GIT_NO_LAZY_FETCH", "0")]),
        "clone with GIT_NO_LAZY_FETCH=0",
    );
    assert_eq!(
        std::fs::read(s.base.join("by-env/big")).unwrap(),
        big_content(1)
    );
    assert!(s.missing_in_main().is_empty(), "main fetched the blob");
    let promisor_packs = std::fs::read_dir(s.main.join("objects/pack"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "promisor"))
        .count();
    assert_eq!(promisor_packs, 2);
    s.git(&s.main, &["fsck"]);
}

#[test]
fn uploadpack_lazy_fetch_config_serves_clients_that_accept_nothing() {
    let Some(s) = server() else { return };
    s.git(&s.main, &["config", "uploadpack.lazyFetch", "true"]);
    ok(
        s.git_clone("by-config", &[], &[]),
        "clone with uploadpack.lazyFetch",
    );
    assert!(s.missing_in_main().is_empty());
}

#[test]
fn grit_clone_by_path_needs_to_accept_the_lop() {
    let Some(s) = server() else { return };
    let main = s.main.display().to_string();
    let refused = run(GRIT, &s.base, &s.base, &["clone", &main, "plain"], &[]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("did not accept"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let lop_url = format!("remote.lop.url={}", s.lop_url());
    s.grit(
        &s.base,
        &[
            "clone",
            "--blobs",
            "auto",
            "--config",
            "promisor.acceptFromServer=All",
            "--config",
            &lop_url,
            "--config",
            "remote.lop.promisor=true",
            &main,
            "accepted",
        ],
    );
    assert_eq!(
        std::fs::read(s.base.join("accepted/big")).unwrap(),
        big_content(1)
    );
}

#[test]
fn grit_serves_a_lop_layout_built_by_git() {
    if !git_at_least(2, 49) {
        eprintln!("SKIP: git older than 2.49 has no promisor-remote capability");
        return;
    }
    // Git's own recipe (t5710): repack with --filter-to and mark the rest promisor.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let git = |dir: &Path, args: &[&str]| ok(run("git", dir, &base, args, &[]), "git");
    let work = base.join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("small"), "small\n").unwrap();
    std::fs::write(work.join("big"), big_content(2)).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "one"]);
    git(
        &base,
        &["clone", "-q", "--bare", "--no-local", "work", "server.git"],
    );
    git(&base, &["init", "-q", "--bare", "lop.git"]);
    let server = base.join("server.git");
    let lop = base.join("lop.git");
    let lop_pack = lop.join("objects/pack/pack");
    git(
        &server,
        &[
            "-c",
            "repack.writeBitmaps=false",
            "repack",
            "-a",
            "-d",
            "--filter=blob:limit=5k",
            &format!("--filter-to={}", lop_pack.display()),
        ],
    );
    for entry in std::fs::read_dir(server.join("objects/pack"))
        .unwrap()
        .flatten()
    {
        if entry.path().extension().is_some_and(|x| x == "pack") {
            std::fs::write(entry.path().with_extension("promisor"), b"").unwrap();
        }
    }
    let lop_url = format!("file://{}", lop.display());
    for (k, v) in [
        ("remote.lop.url", lop_url.as_str()),
        ("remote.lop.promisor", "true"),
        ("promisor.advertise", "true"),
        ("uploadpack.allowFilter", "true"),
    ] {
        git(&server, &["config", k, v]);
    }
    git(&lop, &["config", "uploadpack.allowFilter", "true"]);
    git(&lop, &["config", "uploadpack.allowAnySHA1InWant", "true"]);

    let upload_pack = format!("{GRIT} upload-pack");
    let url = format!("file://{}", server.display());
    let lop_cfg = format!("remote.lop.url={lop_url}");
    ok(
        run(
            "git",
            &base,
            &base,
            &[
                "-c",
                "protocol.version=2",
                "clone",
                "--no-local",
                "--upload-pack",
                &upload_pack,
                "-c",
                "remote.lop.promisor=true",
                "-c",
                &lop_cfg,
                "-c",
                "promisor.acceptfromserver=All",
                "--filter=blob:limit=5k",
                &url,
                "client",
            ],
            &[],
        ),
        "clone of a git-built LOP layout",
    );
    assert_eq!(
        std::fs::read(base.join("client/big")).unwrap(),
        big_content(2)
    );
}

#[test]
fn git_pushes_small_edits_to_an_offloaded_grit_server() {
    let Some(s) = server() else { return };
    let lop_up = format!("{GRIT} upload-pack");
    let lop_url = s.lop_url();
    let extra = s.accepting(&lop_url, Some(&lop_up));
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    ok(s.git_clone("client", &extra, &[]), "accepting clone");
    let client = s.base.join("client");
    // Edit a file next to the offloaded one: the pushed tree still names it.
    std::fs::write(client.join("small"), "edited\n").unwrap();
    s.git(&client, &["commit", "-qam", "edit"]);
    let receive_pack = format!("{GRIT} receive-pack");
    s.git(
        &client,
        &[
            "push",
            "-q",
            "--receive-pack",
            &receive_pack,
            "origin",
            "main",
        ],
    );
    let tip = s.git(&client, &["rev-parse", "HEAD"]);
    assert_eq!(s.git(&s.main, &["rev-parse", "main"]), tip);
    assert_eq!(s.missing_in_main(), [s.big.clone()], "nothing was fetched");
    s.git(&s.main, &["fsck"]);
    s.grit(&s.main, &["fsck"]);

    // Offload again folds the pushed objects into the promisor pack.
    let again = s.grit(&s.main, &["gc", "offload", "--to", "lop", "--json"]);
    assert!(again.contains("\"candidates\":0"), "{again}");
    assert!(!again.contains("\"main_pack\":null"), "{again}");
    s.git(&s.main, &["fsck"]);

    // Git's gc works on the bare repository after a push (no bitmaps).
    std::fs::write(client.join("small"), "edited again\n").unwrap();
    s.git(&client, &["commit", "-qam", "edit again"]);
    s.git(
        &client,
        &[
            "push",
            "-q",
            "--receive-pack",
            &receive_pack,
            "origin",
            "main",
        ],
    );
    s.git(&s.main, &["gc", "--quiet"]);
    s.git(&s.main, &["fsck"]);
    assert_eq!(s.missing_in_main(), [s.big.clone()]);

    // A new accepting clone still gets the large file from the LOP.
    ok(s.git_clone("again", &extra, &[]), "clone after the pushes");
    assert_eq!(
        std::fs::read(s.base.join("again/big")).unwrap(),
        big_content(1)
    );
    assert_eq!(
        std::fs::read_to_string(s.base.join("again/small")).unwrap(),
        "edited again\n"
    );
}

#[test]
fn server_lazy_fetch_still_honors_the_clients_filter() {
    let Some(s) = server() else { return };
    ok(
        s.git_clone(
            "filtered",
            &["--no-checkout", "--filter=blob:limit=5k"],
            &[("GIT_NO_LAZY_FETCH", "0")],
        ),
        "filtered clone with server lazy fetch",
    );
    let client = s.base.join("filtered");
    let missing: Vec<String> = s
        .git(
            &client,
            &["rev-list", "--objects", "--all", "--missing=print"],
        )
        .lines()
        .filter_map(|l| l.strip_prefix('?').map(str::to_owned))
        .collect();
    assert_eq!(missing, [s.big.clone()], "the large blob was filtered out");
}

#[test]
fn v0_want_of_an_offloaded_blob_gets_a_remote_error() {
    let Some(s) = server() else { return };
    let client = s.base.join("v0");
    std::fs::create_dir(&client).unwrap();
    s.git(&client, &["init", "-q"]);
    let upload_pack = format!("{GRIT} upload-pack");
    let url = format!("file://{}", s.main.display());
    let out = run(
        "git",
        &client,
        &s.base,
        &[
            "-c",
            "protocol.version=0",
            "fetch",
            "--upload-pack",
            &upload_pack,
            &url,
            &s.big,
        ],
        &[],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("remote error"), "{stderr}");
    assert!(stderr.contains(&s.big), "{stderr}");
}
