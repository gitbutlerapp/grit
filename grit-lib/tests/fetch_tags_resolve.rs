//! Coverage for [`grit_lib::remote::transfer::resolve_fetch_tags`].

use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::remote::transfer::{resolve_fetch_tags, TagMode};

fn config_with(text: &str) -> ConfigSet {
    let file =
        ConfigFile::parse(std::path::Path::new("cfg"), text, ConfigScope::Local).expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&file);
    set
}

#[test]
fn resolve_fetch_tags_cli_overrides_config() {
    let config = config_with("[remote \"origin\"]\n\ttagOpt = --tags\n");
    assert_eq!(
        resolve_fetch_tags(&config, "origin", Some(false)),
        TagMode::None
    );
    assert_eq!(
        resolve_fetch_tags(&config, "origin", Some(true)),
        TagMode::All
    );
}

#[test]
fn resolve_fetch_tags_honors_remote_tagopt() {
    let config = config_with(
        "[remote \"origin\"]\n\ttagOpt = --no-tags\n[remote \"upstream\"]\n\ttagOpt = --tags\n",
    );
    assert_eq!(resolve_fetch_tags(&config, "origin", None), TagMode::None);
    assert_eq!(resolve_fetch_tags(&config, "upstream", None), TagMode::All);
}

#[test]
fn resolve_fetch_tags_defaults_following() {
    let config = ConfigSet::new();
    assert_eq!(
        resolve_fetch_tags(&config, "origin", None),
        TagMode::Following
    );
}
