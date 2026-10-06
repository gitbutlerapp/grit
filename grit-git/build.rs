//! Copy bundled `upstream_help_synopsis.rs` into `OUT_DIR` for t0450 help parity.
//! Install `git-sh-setup` and `git-sh-i18n` next to the `grit-git` binary (Git exec-path layout).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dst = out_dir.join("upstream_help_synopsis.rs");
    let bundled = manifest_dir.join("upstream_help_synopsis.rs");

    fs::copy(&bundled, &dst).unwrap_or_else(|e| {
        panic!(
            "copy bundled {} to {}: {e}",
            bundled.display(),
            dst.display()
        );
    });
    println!("cargo:rerun-if-changed={}", bundled.display());

    install_shell_libs(&manifest_dir);
}

/// Writes `git-sh-setup` and `git-sh-i18n` into `target/<profile>/` (same directory as `grit-git`).
fn install_shell_libs(manifest_dir: &Path) {
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let target_dir = manifest_dir.join("../target").join(&profile);
    if !target_dir.is_dir() {
        return;
    }

    let shell_lib = manifest_dir.join("shell-lib");
    let git_sh_i18n_src = shell_lib.join("git-sh-i18n.sh");
    let git_sh_setup_src = shell_lib.join("git-sh-setup.sh");
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
