//! Regenerate `git <cmd> -h` synopsis snippets from vendored `git/Documentation/*.adoc` for t0450.
//! Also install `git-sh-setup` and `git-sh-i18n` next to the `grit` binary (Git exec-path layout).
//!
//! When building from a published crate tarball (`cargo package`), the workspace `scripts/` and
//! `git/Documentation` trees are not present; we copy the bundled `upstream_help_synopsis.rs`
//! checked into this crate instead.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dst = out_dir.join("upstream_help_synopsis.rs");

    let script = manifest_dir.join("../scripts/generate-upstream-help-synopsis.py");
    let docs_dir = manifest_dir.join("../git/Documentation");
    let bundled = manifest_dir.join("upstream_help_synopsis.rs");

    // Regenerate from the vendored git docs only when the full source tree AND a
    // usable `python3` are present. Otherwise fall back to the copy checked into
    // this crate. This covers crates.io tarball builds (no `scripts/` /
    // `git/Documentation`) and cross-builds in a container that has the source
    // tree but no `python3` (e.g. the musl release built with `cross`) — there we
    // must NOT hard-fail, since the bundled copy is the source of truth anyway.
    if script.is_file() && docs_dir.is_dir() && try_generate(&script, &dst) {
        println!("cargo:rerun-if-changed={}", script.display());
        println!("cargo:rerun-if-changed={}", docs_dir.display());
    } else {
        fs::copy(&bundled, &dst).unwrap_or_else(|e| {
            panic!(
                "copy bundled {} to {}: {e} (expected when building from crates.io tarball)",
                bundled.display(),
                dst.display()
            );
        });
        println!("cargo:rerun-if-changed={}", bundled.display());
    }

    install_shell_libs(&manifest_dir);
}

/// Run `python3 <script>` to (re)generate `dst`. Returns `true` on success.
///
/// Returns `false` — emitting a `cargo:warning` rather than panicking — when
/// `python3` is unavailable (e.g. a `cross` musl container) or the script fails,
/// so the caller can fall back to the bundled, checked-in copy. The synopsis file
/// only feeds a git-compatibility test, so the bundled copy is a correct,
/// reproducible substitute for a release build.
fn try_generate(script: &Path, dst: &Path) -> bool {
    let out_file = match fs::File::create(dst) {
        Ok(f) => f,
        Err(e) => {
            println!("cargo:warning=create {}: {e}", dst.display());
            return false;
        }
    };
    match Command::new("python3").arg(script).stdout(out_file).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            println!(
                "cargo:warning=generate-upstream-help-synopsis.py failed with {status}; \
                 using bundled upstream_help_synopsis.rs"
            );
            false
        }
        Err(e) => {
            println!(
                "cargo:warning=python3 unavailable ({e}); \
                 using bundled upstream_help_synopsis.rs"
            );
            false
        }
    }
}

/// Writes `git-sh-setup` and `git-sh-i18n` into `target/<profile>/` (same directory as `grit`).
fn install_shell_libs(manifest_dir: &Path) {
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let target_dir = manifest_dir.join("../target").join(&profile);
    if !target_dir.is_dir() {
        return;
    }

    let git_sh_i18n_src = manifest_dir.join("../git/git-sh-i18n.sh");
    let git_sh_setup_src = manifest_dir.join("../git/git-sh-setup.sh");
    if !git_sh_i18n_src.is_file() || !git_sh_setup_src.is_file() {
        return;
    }

    let shell_path = "/bin/sh";
    let diff = "diff";
    let pager_env = "LESS=FRX LV=-c";
    let local_edir = "/usr/local/share/locale";

    let i18n = fs::read_to_string(&git_sh_i18n_src).unwrap_or_else(|e| {
        panic!("read {}: {e}", git_sh_i18n_src.display());
    });
    let i18n_out = i18n
        .replace("@LOCALEDIR@", local_edir)
        .replace("@USE_GETTEXT_SCHEME@", "");
    let i18n_dst = target_dir.join("git-sh-i18n");
    fs::write(&i18n_dst, i18n_out).unwrap_or_else(|e| panic!("write {}: {e}", i18n_dst.display()));

    let setup = fs::read_to_string(&git_sh_setup_src).unwrap_or_else(|e| {
        panic!("read {}: {e}", git_sh_setup_src.display());
    });
    let setup_out = setup
        .replace("# @BROKEN_PATH_FIX@", "")
        .replace("@PAGER_ENV@", pager_env)
        .replace("@DIFF@", diff)
        .replace("#! /bin/sh", &format!("#!{shell_path}"))
        .replace("#!/bin/sh", &format!("#!{shell_path}"));
    let setup_dst = target_dir.join("git-sh-setup");
    let mut f = fs::File::create(&setup_dst)
        .unwrap_or_else(|e| panic!("create {}: {e}", setup_dst.display()));
    f.write_all(setup_out.as_bytes())
        .unwrap_or_else(|e| panic!("write {}: {e}", setup_dst.display()));

    println!("cargo:rerun-if-changed={}", git_sh_i18n_src.display());
    println!("cargo:rerun-if-changed={}", git_sh_setup_src.display());
}
