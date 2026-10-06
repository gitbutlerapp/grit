//! Comprehensive matrix tests for `grit_lib::credentials`.
//!
//! This file exercises the full Git-compatible credential layer beyond the
//! happy-path coverage in `tests/credentials.rs`:
//!
//!   * [`Credential`] wire-format round-tripping, including preservation of
//!     unrecognized / multi-valued (`capability[]`) keys in `extra`, CRLF
//!     tolerance, blank-line record termination, and canonical field order;
//!   * the `Credential` accessors (`is_complete`, `target_url`, `parse_bytes`);
//!   * [`use_http_path`] config resolution (global and URL-scoped);
//!   * [`HelperCredentialProvider`] driving REAL helper programs:
//!       - a shell (`!cmd`) helper (fill + store/erase dispatch, arg forwarding);
//!       - the external `git-credential-store` binary against an on-disk store
//!         file, with the full fill -> approve(store) -> reject(erase) lifecycle
//!         cross-checked against the system `git credential-store` reading the
//!         very same file;
//!   * `credential.<url>.helper` URL scoping (match, non-match, wildcard, and
//!     the empty-value "reset" that clears the helper list);
//!   * multi-helper chaining (a first helper that yields nothing, a second that
//!     completes) and the `quit=1` short-circuit;
//!   * the typed NON-INTERACTIVE failure when no helper can supply creds — proven
//!     to return (not hang) via a watchdog thread;
//!   * HTTP `401` -> `fill` -> `Authorization: Basic` retry against an authed
//!     `grit-http-server`, driven through `HelperCredentialProvider` + a real
//!     shell helper, asserting the fetch lands real objects (fsck-clean) and the
//!     wrong-credential case fails with the typed [`Error::Auth`].
//!
//! Real fixtures throughout: system `git`, on-disk repos / credential stores,
//! the external `git-credential-store` helper, and (for the HTTP case) the
//! `grit-http-server` binary. Cases whose fixture is genuinely unavailable SKIP
//! cleanly, but every test makes a real assertion on its happy path.
//!
//! Gated on `http-ureq` only for the HTTP-401 case (the default `UreqHttpClient`
//! lives behind that feature); the non-HTTP tests compile unconditionally.
//!   cargo test -p grit-lib --features http-ureq --test matrix_credentials

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::Once;
use std::time::Duration;

use grit_lib::config::ConfigSet;
use grit_lib::credentials::{
    use_http_path, Credential, CredentialProvider, HelperCredentialProvider,
    NON_INTERACTIVE_MESSAGE,
};

// ---------------------------------------------------------------------------
// Shared fixture helpers (mirrors tests/credentials.rs so the harness matches).
// ---------------------------------------------------------------------------

/// Run `git` in `dir`; returns `None` if git is unavailable.
fn git(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()
}

/// Initialize a repo in `dir`. Returns `false` if git is unavailable.
fn init_repo(dir: &Path) -> bool {
    matches!(git(dir, &["init", "-q"]), Some(out) if out.status.success())
}

/// Write an executable shell script.
fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod script");
}

static ISOLATE: Once = Once::new();

/// Detach config loading from the developer's real `~/.gitconfig` /
/// `/etc/gitconfig`. `ConfigSet::load` honors `GIT_CONFIG_GLOBAL` /
/// `GIT_CONFIG_SYSTEM`; pointing them at `/dev/null` yields no entries, so the
/// only `credential.*` config the provider sees is what the test wrote into the
/// repo-local config. Process-wide-once is race-free under parallel tests.
fn isolate_global_config() {
    ISOLATE.call_once(|| {
        // SAFETY: set once, to a constant, before any test loads config.
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    });
}

/// Load the repo-local config (no global/system) for the worktree at `dir`.
fn load_config(dir: &Path) -> ConfigSet {
    isolate_global_config();
    let git_dir = dir.join(".git");
    ConfigSet::load(Some(&git_dir), false).expect("load config")
}

/// Apply `key=value` config via `git config --local`, then load the config.
fn build_config(dir: &Path, kvs: &[(&str, &str)]) -> ConfigSet {
    for (k, v) in kvs {
        let out = git(dir, &["config", "--local", k, v]).expect("git config");
        assert!(out.status.success(), "git config {k} failed: {out:?}");
    }
    load_config(dir)
}

fn sample_target(host: &str) -> Credential {
    Credential {
        protocol: Some("https".into()),
        host: Some(host.into()),
        ..Default::default()
    }
}

/// Locate `git-credential-store` so we can drive the REAL external helper.
/// Returns `None` when no such binary is reachable (the case then SKIPs).
fn find_git_credential_store() -> Option<PathBuf> {
    // 1. Git's exec-path (where helpers actually live).
    if let Some(out) = Command::new("git").arg("--exec-path").output().ok() {
        if out.status.success() {
            let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
            let cand = dir.join("git-credential-store");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // 2. Common install locations.
    for cand in [
        "/usr/libexec/git-core/git-credential-store",
        "/opt/homebrew/opt/git/libexec/git-core/git-credential-store",
        "/Library/Developer/CommandLineTools/usr/libexec/git-core/git-credential-store",
        "/usr/lib/git-core/git-credential-store",
    ] {
        let p = PathBuf::from(cand);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Run a closure on a worker thread with a hard wall-clock budget; panics if it
/// does not return in time. Used to prove the non-interactive path never blocks
/// on a TTY/askpass.
fn run_with_watchdog<T, F>(secs: u64, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("operation hung past the watchdog budget (interactive prompt?)")
}

// ---------------------------------------------------------------------------
// 1. Credential wire format & accessors (pure, no fixtures required).
// ---------------------------------------------------------------------------

#[test]
fn wire_format_preserves_extra_and_multivalued_keys_round_trip() {
    // Recognized fields + an unknown key + a repeated multi-valued `capability[]`
    // and a helper directive (`authtype`) that must survive a round-trip in order.
    let input = "protocol=https\n\
                 host=example.com\n\
                 username=alice\n\
                 password=secret\n\
                 capability[]=authtype\n\
                 capability[]=state\n\
                 authtype=Bearer\n\
                 password_expiry_utc=1700000000\n";
    let cred = Credential::parse(input);

    assert_eq!(cred.protocol.as_deref(), Some("https"));
    assert_eq!(cred.host.as_deref(), Some("example.com"));
    assert_eq!(cred.username.as_deref(), Some("alice"));
    assert_eq!(cred.password.as_deref(), Some("secret"));

    // Multi-valued key appears twice, in order; single unknown keys preserved.
    let caps: Vec<&str> = cred
        .extra
        .iter()
        .filter(|(k, _)| k == "capability[]")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(
        caps,
        vec!["authtype", "state"],
        "capability[] order preserved"
    );
    assert!(cred
        .extra
        .iter()
        .any(|(k, v)| k == "authtype" && v == "Bearer"));
    assert!(cred
        .extra
        .iter()
        .any(|(k, v)| k == "password_expiry_utc" && v == "1700000000"));

    // Exact byte round-trip (canonical field order + extras in stored order).
    assert_eq!(cred.serialize(), input, "serialize must round-trip exactly");
}

#[test]
fn parse_stops_at_blank_line_and_tolerates_crlf() {
    // CRLF line endings + a blank line terminator; everything after the blank
    // line (the `injected=` directive) must be ignored, matching Git.
    let input =
        "protocol=https\r\nhost=h.example\r\nusername=u\r\npassword=p\r\n\r\ninjected=evil\r\n";
    let cred = Credential::parse(input);
    assert_eq!(cred.host.as_deref(), Some("h.example"));
    assert_eq!(cred.username.as_deref(), Some("u"));
    assert_eq!(cred.password.as_deref(), Some("p"));
    assert!(
        cred.extra.iter().all(|(k, _)| k != "injected"),
        "fields after the blank record terminator must not be parsed: {:?}",
        cred.extra
    );
}

#[test]
fn parse_bytes_matches_parse_and_lines_without_equals_ignored() {
    let bytes = b"protocol=https\nhost=b.example\nthis line has no equals sign\nusername=bob\npassword=pw\n";
    let cred = Credential::parse_bytes(bytes);
    assert_eq!(cred.host.as_deref(), Some("b.example"));
    assert_eq!(cred.username.as_deref(), Some("bob"));
    assert_eq!(cred.password.as_deref(), Some("pw"));
    // The malformed line is silently dropped (not an extra, not a panic).
    assert!(cred
        .extra
        .iter()
        .all(|(k, _)| !k.contains("this line has no equals sign")));
    // parse_bytes and parse agree on identical input.
    assert_eq!(cred, Credential::parse(&String::from_utf8_lossy(bytes)));
}

#[test]
fn is_complete_requires_nonempty_user_and_password() {
    let mut c = Credential {
        protocol: Some("https".into()),
        host: Some("h".into()),
        ..Default::default()
    };
    assert!(!c.is_complete(), "no user/pass");
    c.username = Some("u".into());
    assert!(!c.is_complete(), "user only");
    c.password = Some(String::new());
    assert!(!c.is_complete(), "empty password is not complete");
    c.password = Some("p".into());
    assert!(c.is_complete(), "user + non-empty password is complete");
    c.username = Some(String::new());
    assert!(!c.is_complete(), "empty username is not complete");
}

#[test]
fn target_url_prefers_explicit_url_then_reconstructs_with_userinfo() {
    // Explicit url wins verbatim.
    let with_url = Credential {
        protocol: Some("https".into()),
        host: Some("ignored.example".into()),
        url: Some("https://github.com/owner/repo.git".into()),
        ..Default::default()
    };
    assert_eq!(
        with_url.target_url().as_deref(),
        Some("https://github.com/owner/repo.git")
    );

    // Reconstructed from fields, including username -> user@host and path join.
    let reconstructed = Credential {
        protocol: Some("https".into()),
        host: Some("git.example".into()),
        username: Some("alice".into()),
        path: Some("team/proj.git".into()),
        ..Default::default()
    };
    assert_eq!(
        reconstructed.target_url().as_deref(),
        Some("https://alice@git.example/team/proj.git")
    );

    // Missing host -> cannot reconstruct.
    let bare = Credential {
        protocol: Some("https".into()),
        ..Default::default()
    };
    assert_eq!(bare.target_url(), None);
}

// ---------------------------------------------------------------------------
// 2. use_http_path config resolution (real on-disk config).
// ---------------------------------------------------------------------------

#[test]
fn use_http_path_reads_global_and_url_scoped_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("SKIP: system git unavailable");
        return;
    }

    // Default (unset) -> false.
    let cfg0 = load_config(dir);
    assert!(
        !use_http_path(&cfg0, Some("https://github.com/o/r.git")),
        "unset credential.useHttpPath defaults to false"
    );

    // Global true -> true for any URL.
    let cfg1 = build_config(dir, &[("credential.useHttpPath", "true")]);
    assert!(use_http_path(&cfg1, Some("https://github.com/o/r.git")));
    assert!(use_http_path(&cfg1, None));

    // URL-scoped override: off globally but on for one host.
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let dir2 = tmp2.path();
    assert!(init_repo(dir2));
    let cfg2 = build_config(
        dir2,
        &[
            ("credential.useHttpPath", "false"),
            ("credential.https://scoped.example.useHttpPath", "true"),
        ],
    );
    assert!(
        use_http_path(&cfg2, Some("https://scoped.example/o/r.git")),
        "URL-scoped useHttpPath=true must apply to the matching host"
    );
    assert!(
        !use_http_path(&cfg2, Some("https://other.example/o/r.git")),
        "non-matching host falls back to the global false"
    );
}

// ---------------------------------------------------------------------------
// 3. Shell (!cmd) helper: fill, arg forwarding, store/erase dispatch.
// ---------------------------------------------------------------------------

#[test]
fn shell_helper_fills_and_forwards_configured_args_and_dispatches_actions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("SKIP: system git unavailable");
        return;
    }

    // For the shell (`!cmd`) form Git appends the action AFTER the configured
    // args, so the helper sees `<configured-arg> <action>` ($1=arg, $2=action),
    // exactly as `git credential fill` invokes a `!cmd MARK` helper. Record both
    // so we prove the action dispatch AND the arg forwarding.
    let record = dir.join("record.log");
    let helper = dir.join("argful.sh");
    write_script(
        &helper,
        &format!(
            "#!/bin/sh\n\
             echo \"action=$2 arg=$1\" >> {record}\n\
             if [ \"$2\" = get ]; then\n\
               echo username=carol\n\
               echo password=pw-carol\n\
             fi\n",
            record = record.display()
        ),
    );

    // Configured value: shell form with a trailing literal argument `MARK`.
    let helper_value = format!("!{} MARK", helper.display());
    let cfg = build_config(dir, &[("credential.helper", &helper_value)]);
    let provider = HelperCredentialProvider::new(cfg);

    let filled = provider
        .fill(&sample_target("argful.example"))
        .expect("fill via shell helper");
    assert_eq!(filled.username.as_deref(), Some("carol"));
    assert_eq!(filled.password.as_deref(), Some("pw-carol"));

    let cred = Credential {
        username: Some("carol".into()),
        password: Some("pw-carol".into()),
        ..sample_target("argful.example")
    };
    provider.approve(&cred).expect("approve -> store");
    provider.reject(&cred).expect("reject -> erase");

    let log = fs::read_to_string(&record).expect("record log");
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        lines.contains(&"action=get arg=MARK"),
        "fill must invoke `get` with the forwarded arg, got {lines:?}"
    );
    assert!(
        lines.contains(&"action=store arg=MARK"),
        "approve must invoke `store` with the forwarded arg, got {lines:?}"
    );
    assert!(
        lines.contains(&"action=erase arg=MARK"),
        "reject must invoke `erase` with the forwarded arg, got {lines:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. REAL external git-credential-store helper: full lifecycle + git cross-check.
// ---------------------------------------------------------------------------

#[test]
fn external_credential_store_helper_full_lifecycle_cross_checked_with_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("SKIP: system git unavailable");
        return;
    }
    let Some(store_bin) = find_git_credential_store() else {
        eprintln!("SKIP: git-credential-store binary not found");
        return;
    };

    // An on-disk store file the external helper reads/writes. We point the
    // helper at it explicitly (`--file=`) so the test is hermetic.
    let store_file = dir.join("store.txt");
    // Configure credential.helper = "git-credential-store --file=<path>".
    // The provider resolves the bare `git-credential-store` name across Git's
    // exec-path; passing the absolute path keeps it robust.
    let helper_value = format!("{} --file={}", store_bin.display(), store_file.display());
    let cfg = build_config(dir, &[("credential.helper", &helper_value)]);
    let provider = HelperCredentialProvider::new(cfg);

    let target = Credential {
        protocol: Some("https".into()),
        host: Some("store.example.com".into()),
        ..Default::default()
    };
    let full = Credential {
        username: Some("dave".into()),
        password: Some("p@ss-w0rd".into()),
        ..target.clone()
    };

    // Empty store -> fill cannot complete -> typed non-interactive error.
    let err = provider
        .fill(&target)
        .expect_err("empty store yields no creds");
    assert!(
        err.to_string().contains(NON_INTERACTIVE_MESSAGE),
        "empty store should surface the non-interactive error, got: {err}"
    );

    // approve(store) writes the credential to the store file.
    provider
        .approve(&full)
        .expect("approve -> store writes file");
    let store_contents = fs::read_to_string(&store_file).expect("store file written");
    assert!(
        store_contents.contains("https://dave:p%40ss-w0rd@store.example.com")
            || store_contents.contains("https://dave:p@ss-w0rd@store.example.com"),
        "store file should hold the stored credential, got: {store_contents:?}"
    );

    // fill now completes from the stored credential.
    let filled = provider.fill(&target).expect("fill from stored credential");
    assert_eq!(filled.username.as_deref(), Some("dave"));
    assert_eq!(filled.password.as_deref(), Some("p@ss-w0rd"));
    // Target fields preserved.
    assert_eq!(filled.host.as_deref(), Some("store.example.com"));

    // CROSS-CHECK: the system `git credential-store` reading the SAME file must
    // return the identical credential (proves wire compatibility with Git).
    let git_get = Command::new("git")
        .args([
            "credential-store",
            &format!("--file={}", store_file.display()),
            "get",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"protocol=https\nhost=store.example.com\n\n")?;
            child.wait_with_output()
        })
        .expect("run git credential-store get");
    let git_cred = Credential::parse_bytes(&git_get.stdout);
    assert_eq!(
        git_cred.username.as_deref(),
        Some("dave"),
        "system git must read the same stored username"
    );
    assert_eq!(git_cred.password.as_deref(), Some("p@ss-w0rd"));

    // reject(erase) removes it; fill goes back to the typed non-interactive error.
    provider
        .reject(&full)
        .expect("reject -> erase removes credential");
    let after_erase = fs::read_to_string(&store_file).unwrap_or_default();
    assert!(
        !after_erase.contains("store.example.com"),
        "erase should remove the credential, store still has it: {after_erase:?}"
    );
    let err2 = provider
        .fill(&target)
        .expect_err("fill after erase yields no creds");
    assert!(err2.to_string().contains(NON_INTERACTIVE_MESSAGE));
}

// ---------------------------------------------------------------------------
// 5. URL-scoped credential.<url>.helper matching: match / wildcard / reset.
// ---------------------------------------------------------------------------

#[test]
fn url_scoped_helper_match_nonmatch_wildcard_and_reset() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("SKIP: system git unavailable");
        return;
    }

    // A wildcard-scoped helper that completes only for *.corp.example hosts.
    let helper = dir.join("corp.sh");
    write_script(
        &helper,
        "#!/bin/sh\n\
         if [ \"$1\" = get ]; then\n\
           echo username=corp-user\n\
           echo password=corp-pw\n\
         fi\n",
    );
    let helper_value = format!("!{}", helper.display());
    let cfg = build_config(
        dir,
        &[("credential.https://*.corp.example.helper", &helper_value)],
    );
    let provider = HelperCredentialProvider::new(cfg);

    // Wildcard match -> filled.
    let inside = provider
        .fill(&sample_target("git.corp.example"))
        .expect("wildcard scope matches *.corp.example");
    assert_eq!(inside.username.as_deref(), Some("corp-user"));

    // Different host -> scope does not apply -> typed non-interactive error.
    let outside = provider
        .fill(&sample_target("github.com"))
        .expect_err("non-matching host gets no scoped helper");
    assert!(outside.to_string().contains(NON_INTERACTIVE_MESSAGE));

    // RESET semantics: a later empty `credential.helper` clears any prior list.
    // Configure an unscoped helper then reset it; fill must yield nothing.
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let dir2 = tmp2.path();
    assert!(init_repo(dir2));
    let real = dir2.join("real.sh");
    write_script(
        &real,
        "#!/bin/sh\nif [ \"$1\" = get ]; then echo username=u; echo password=p; fi\n",
    );
    let real_value = format!("!{}", real.display());
    // First set a helper, then append an empty reset entry (Git clears the list).
    let out = git(
        dir2,
        &["config", "--local", "credential.helper", &real_value],
    )
    .expect("git config helper");
    assert!(out.status.success());
    let out = git(
        dir2,
        &["config", "--local", "--add", "credential.helper", ""],
    )
    .expect("git config reset");
    assert!(out.status.success());
    let cfg2 = load_config(dir2);
    let provider2 = HelperCredentialProvider::new(cfg2);
    let reset_err = provider2
        .fill(&sample_target("anything.example"))
        .expect_err("an empty credential.helper resets the list -> no creds");
    assert!(
        reset_err.to_string().contains(NON_INTERACTIVE_MESSAGE),
        "empty-value reset should clear helpers, got: {reset_err}"
    );
}

// ---------------------------------------------------------------------------
// 6. Multi-helper chaining and quit short-circuit.
// ---------------------------------------------------------------------------

#[test]
fn multiple_helpers_chain_until_complete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("SKIP: system git unavailable");
        return;
    }

    // First helper supplies ONLY a username; second supplies the password.
    // The provider must merge across helpers (Git stops once complete).
    let first = dir.join("user_only.sh");
    write_script(
        &first,
        "#!/bin/sh\nif [ \"$1\" = get ]; then echo username=chain-user; fi\n",
    );
    let second = dir.join("pass_only.sh");
    write_script(
        &second,
        "#!/bin/sh\nif [ \"$1\" = get ]; then echo password=chain-pass; fi\n",
    );

    let first_value = format!("!{}", first.display());
    let second_value = format!("!{}", second.display());
    for (k, v) in [
        ("credential.helper", first_value.as_str()),
        ("credential.helper", second_value.as_str()),
    ] {
        let out = git(dir, &["config", "--local", "--add", k, v]).expect("git config add");
        assert!(out.status.success());
    }
    let cfg = load_config(dir);
    let provider = HelperCredentialProvider::new(cfg);

    let filled = provider
        .fill(&sample_target("chain.example"))
        .expect("two helpers together complete the credential");
    assert_eq!(filled.username.as_deref(), Some("chain-user"));
    assert_eq!(filled.password.as_deref(), Some("chain-pass"));
}

#[test]
fn helper_quit_short_circuits_with_typed_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("SKIP: system git unavailable");
        return;
    }

    // First helper says quit=1 -> the provider must stop and NOT consult the
    // second (which would otherwise complete the credential).
    let quitter = dir.join("quit.sh");
    write_script(
        &quitter,
        "#!/bin/sh\nif [ \"$1\" = get ]; then echo quit=1; fi\n",
    );
    let would_fill = dir.join("would_fill.sh");
    write_script(
        &would_fill,
        "#!/bin/sh\nif [ \"$1\" = get ]; then echo username=nope; echo password=nope; fi\n",
    );
    for v in [
        format!("!{}", quitter.display()),
        format!("!{}", would_fill.display()),
    ] {
        let out = git(
            dir,
            &["config", "--local", "--add", "credential.helper", &v],
        )
        .expect("git config add");
        assert!(out.status.success());
    }
    let cfg = load_config(dir);
    let provider = HelperCredentialProvider::new(cfg);

    let err = provider
        .fill(&sample_target("quit.example"))
        .expect_err("quit=1 must short-circuit before the second helper fills");
    assert!(
        err.to_string().contains("quit"),
        "expected a quit-signalled error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// 7. Typed non-interactive failure proven to NOT hang (watchdog).
// ---------------------------------------------------------------------------

#[test]
fn no_helper_fails_non_interactively_without_hanging() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !init_repo(dir) {
        eprintln!("SKIP: system git unavailable");
        return;
    }
    let cfg = load_config(dir);
    let provider = HelperCredentialProvider::new(cfg);
    let target = sample_target("nohelper.example");

    // Run on a watchdog thread; if fill blocked on /dev/tty this would time out.
    let err = run_with_watchdog(10, move || {
        provider
            .fill(&target)
            .expect_err("no helper -> typed error")
            .to_string()
    });
    assert!(
        err.contains(NON_INTERACTIVE_MESSAGE),
        "expected the non-interactive message, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// 8. HTTP 401 -> fill -> Basic retry, driven by HelperCredentialProvider.
// ---------------------------------------------------------------------------

#[cfg(feature = "http-ureq")]
mod http_401 {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Stdio};
    use std::time::Instant;

    use grit_lib::error::Error;
    use grit_lib::fetch::NoProgress;
    use grit_lib::refs::resolve_ref;
    use grit_lib::transfer::{FetchOptions, TagMode};
    use grit_lib::transport::http::http_fetch;
    use grit_lib::transport::http::ureq_client::UreqHttpClient;

    const USER: &str = "alice";
    const PASS: &str = "s3cr3t";

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8 git output")
    }

    fn free_port() -> Option<u16> {
        // Dedup port handout per process so a concurrent caller never accepts a
        // port another test already reserved (closes the bind-before-spawn TOCTOU
        // that let two servers share a port under high test parallelism).
        static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
        let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..200 {
            let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
            let p = l.local_addr().ok()?.port();
            drop(l);
            if !used.contains(&p) {
                used.push(p);
                return Some(p);
            }
        }
        None
    }

    fn find_binary(name: &str) -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let deps = exe.parent()?;
        let profile = deps.parent()?;
        for cand in [profile.join(name), deps.join(name)] {
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    }

    fn wait_ready(port: u16) -> bool {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    struct ServerGuard(Child);
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn mirror_bare(work: &Path, root: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let bare = root.join(name);
        git_ok(
            work,
            &[
                "clone",
                "-q",
                "--bare",
                ".",
                bare.to_str().expect("utf8 path"),
            ],
        );
        git_ok(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        bare
    }

    /// Build a `HelperCredentialProvider` whose configured shell helper emits the
    /// given user/pass on `get` and records each invoked action to `record`.
    fn helper_provider(
        cfg_dir: &Path,
        user: &str,
        pass: &str,
        record: &Path,
    ) -> HelperCredentialProvider {
        let helper = cfg_dir.join("http_helper.sh");
        write_script(
            &helper,
            &format!(
                "#!/bin/sh\n\
                 echo \"$1\" >> {record}\n\
                 if [ \"$1\" = get ]; then\n\
                   echo username={user}\n\
                   echo password={pass}\n\
                 fi\n",
                record = record.display(),
                user = user,
                pass = pass
            ),
        );
        let helper_value = format!("!{}", helper.display());
        let cfg = build_config(cfg_dir, &[("credential.helper", &helper_value)]);
        HelperCredentialProvider::new(cfg)
    }

    #[test]
    fn http_401_fill_basic_retry_with_helper_provider() {
        // HelperCredentialProvider must be usable as the HTTP client's provider.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HelperCredentialProvider>();

        let (Some(grit_bin), Some(server_bin)) =
            (find_binary("grit-git"), find_binary("grit-http-server"))
        else {
            eprintln!("SKIP: grit / grit-http-server binary not found (build them first)");
            return;
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        // Source repo: a couple of commits on main.
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git_ok(&work, &["init", "-q", "-b", "main", "."]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        git_ok(&work, &["add", "a.txt"]);
        git_ok(&work, &["commit", "-q", "-m", "c1"]);
        std::fs::write(work.join("b.txt"), "two\n").unwrap();
        git_ok(&work, &["add", "b.txt"]);
        git_ok(&work, &["commit", "-q", "-m", "c2"]);

        let root = tmp.path().join("srv");
        let source = mirror_bare(&work, &root, "repo.git");
        let main_hex = git_ok(&source, &["rev-parse", "refs/heads/main"])
            .trim()
            .to_string();

        let Some(port) = free_port() else {
            eprintln!("SKIP: no free port");
            return;
        };
        let child = Command::new(&server_bin)
            .arg("--root")
            .arg(&root)
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--require-auth")
            .arg(format!("{USER}:{PASS}"))
            .env("GUST_BIN", &grit_bin)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(child) = child else {
            eprintln!("SKIP: could not spawn grit-http-server");
            return;
        };
        let _guard = ServerGuard(child);
        if !wait_ready(port) {
            eprintln!("SKIP: grit-http-server did not become ready");
            return;
        }
        let url = format!("http://127.0.0.1:{port}/repo.git");

        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::None,
            ..Default::default()
        };

        // --- Wrong credentials: the helper supplies a bad password; the 401 ->
        // fill -> Basic retry still fails, surfacing the typed Error::Auth. ------
        {
            let cfg_dir = tmp.path().join("wrongcfg");
            std::fs::create_dir_all(&cfg_dir).unwrap();
            assert!(init_repo(&cfg_dir));
            let rec = cfg_dir.join("rec.log");
            let provider = helper_provider(&cfg_dir, USER, "wrong-pass", &rec);

            let local = tmp.path().join("wrong");
            std::fs::create_dir_all(&local).unwrap();
            git_ok(&local, &["init", "-q", "-b", "main", "."]);
            let local_git = local.join(".git");

            let client =
                UreqHttpClient::with_credentials(Box::new(provider)).with_git_protocol("version=2");
            let err = http_fetch(&client, &local_git, &url, &opts, &mut NoProgress)
                .expect_err("wrong creds from helper must fail typed, not hang");
            assert!(
                matches!(err, Error::Auth(_)),
                "expected Error::Auth for wrong helper creds, got: {err:?}"
            );
            // The helper was actually consulted (a `get` was recorded).
            let log = std::fs::read_to_string(&rec).unwrap_or_default();
            assert!(
                log.lines().any(|l| l == "get"),
                "helper `get` should have been invoked on the 401, log: {log:?}"
            );
        }

        // --- Right credentials: 401 -> fill -> Basic retry succeeds; the real
        // objects land and the pack fsck's clean. ------------------------------
        let cfg_dir = tmp.path().join("okcfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        assert!(init_repo(&cfg_dir));
        let rec = cfg_dir.join("rec.log");
        let provider = helper_provider(&cfg_dir, USER, PASS, &rec);

        let local = tmp.path().join("ok");
        std::fs::create_dir_all(&local).unwrap();
        git_ok(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");

        let client =
            UreqHttpClient::with_credentials(Box::new(provider)).with_git_protocol("version=2");
        http_fetch(&client, &local_git, &url, &opts, &mut NoProgress)
            .expect("authed fetch via helper-filled Basic creds must succeed");

        // The helper supplied a credential on the 401 (a `get` recorded).
        let log = std::fs::read_to_string(&rec).unwrap_or_default();
        assert!(
            log.lines().any(|l| l == "get"),
            "helper `get` should fire on the 401, log: {log:?}"
        );

        // The fetched ref matches the source tip (cross-check vs system git).
        let fetched = resolve_ref(&local_git, "refs/remotes/origin/main")
            .expect("origin/main landed after authed fetch");
        assert_eq!(fetched.to_hex(), main_hex, "fetched tip must match source");

        // The fetched pack fsck's clean.
        let fsck = Command::new("git")
            .current_dir(&local)
            .args(["fsck", "--no-dangling"])
            .output()
            .expect("run git fsck");
        assert!(
            fsck.status.success(),
            "git fsck failed after helper-authed fetch: {}",
            String::from_utf8_lossy(&fsck.stderr)
        );
    }
}
