//! Workspace binaries that tests run as subprocesses.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The binary `name` that cargo built next to the running test, in
/// `target/<profile>/` or `target/<profile>/deps/`, if it is there.
#[must_use]
pub fn sibling_binary(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().ok()?;
    let deps = exe.parent()?;
    let profile = deps.parent()?;
    [profile.join(&file), deps.join(&file)]
        .into_iter()
        .find(|p| p.is_file())
}

/// The `grit-http-server` binary, for tests that need a smart-HTTP server.
///
/// `cargo test --workspace` builds it along with the tests, so this normally
/// only finds it. A narrower run (`cargo test -p grit-cli`) may not have it;
/// then it is built once with `cargo build -p grit-http-server`, which compiles
/// grit-lib again with that crate's features.
///
/// # Panics
///
/// When the binary is missing and cannot be built.
pub fn grit_http_server_binary() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(found) = sibling_binary("grit-http-server") {
            return found;
        }
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = Command::new("cargo")
            .args(["build", "-q", "-p", "grit-http-server"])
            .current_dir(&workspace)
            .status()
            .unwrap_or_else(|e| panic!("spawn cargo build -p grit-http-server: {e}"));
        assert!(status.success(), "cargo build -p grit-http-server failed");
        sibling_binary("grit-http-server")
            .unwrap_or_else(|| panic!("grit-http-server missing after building it"))
    })
    .clone()
}
