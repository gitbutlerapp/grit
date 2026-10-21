//! Coverage for [`grit_lib::remote::transfer::resolve_fetch_prune`].

use std::path::Path;

use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::remote::transfer::resolve_fetch_prune;

fn config_from(text: &str) -> ConfigSet {
    let file = ConfigFile::parse(Path::new("cfg"), text, ConfigScope::Local).expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&file);
    set
}

#[test]
fn resolve_fetch_prune_cli_overrides_config() {
    let config = config_from("[fetch]\n\tprune = true\n[remote \"origin\"]\n\tprune = true\n");
    assert!(!resolve_fetch_prune(&config, "origin", Some(false)).expect("resolve"));
    assert!(resolve_fetch_prune(&config, "origin", Some(true)).expect("resolve"));
}

#[test]
fn resolve_fetch_prune_remote_before_fetch() {
    let config = config_from("[fetch]\n\tprune = false\n[remote \"upstream\"]\n\tprune = true\n");
    assert!(resolve_fetch_prune(&config, "upstream", None).expect("resolve"));
    assert!(!resolve_fetch_prune(&config, "origin", None).expect("resolve"));
}

#[test]
fn resolve_fetch_prune_fetch_key_when_remote_unset() {
    let config = config_from("[fetch]\n\tprune = yes\n");
    assert!(resolve_fetch_prune(&config, "origin", None).expect("resolve"));
}

#[test]
fn resolve_fetch_prune_defaults_false() {
    let config = ConfigSet::new();
    assert!(!resolve_fetch_prune(&config, "origin", None).expect("resolve"));
}
