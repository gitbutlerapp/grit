// API docs: https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use std::path::Path;

fn main() -> grit_lib::error::Result<()> {
    let file = ConfigFile::parse(
        Path::new(".git/config"),
        "[user]\n\tname = Ada\n[core]\n\tbare = false\n",
        ConfigScope::Local,
    )?;
    let mut config = ConfigSet::new();
    config.merge(&file);

    println!(
        "user.name = {}",
        config.get("user.name").unwrap_or_default()
    );
    println!("core.bare = {:?}", config.get_bool("core.bare"));
    Ok(())
}
