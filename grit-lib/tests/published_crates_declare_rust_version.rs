//! Issue #2227: published crates must inherit workspace MSRV for crates.io.

#[test]
fn published_crates_declare_rust_version() {
    const MSRV: &str = "1.95";
    let workspace = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.toml"));
    assert!(
        workspace.contains(&format!("rust-version = \"{MSRV}\"")),
        "workspace [workspace.package] must set rust-version = \"{MSRV}\""
    );

    let published: &[(&str, &str)] = &[
        ("grit-lib", include_str!("../Cargo.toml")),
        ("grit-cli", include_str!("../../grit-cli/Cargo.toml")),
        (
            "grit-protocol",
            include_str!("../../grit-protocol/Cargo.toml"),
        ),
        (
            "grit-http-server",
            include_str!("../../grit-http-server/Cargo.toml"),
        ),
    ];
    for (name, manifest) in published {
        assert!(
            manifest.contains("rust-version.workspace = true"),
            "{name} must set rust-version.workspace = true so crates.io carries MSRV"
        );
    }
}
