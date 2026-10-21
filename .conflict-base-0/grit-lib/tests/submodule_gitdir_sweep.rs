//! Exercise submodule gitdir path helpers for line coverage.

use grit_lib::config::gitmodules::gitdir::{
    compute_default_submodule_gitdir, ensure_submodule_gitdir_config,
    submodule_gitdir_filesystem_path, submodule_gitdir_outer_conflict, submodule_modules_git_dir,
    submodule_path_config_enabled, validate_submodule_path,
};
use grit_lib::config::{ConfigFile, ConfigScope};
use grit_lib::repo::{InitOptions, Repository};
use std::fs;
use tempfile::TempDir;

#[test]
fn submodule_gitdir_path_matrix() -> grit_lib::error::Result<()> {
    let dir = TempDir::new().map_err(grit_lib::error::Error::Io)?;
    let wt = dir.path();
    Repository::init(wt, &InitOptions::default())?;
    let git_dir = wt.join(".git");
    fs::write(
        git_dir.join("config"),
        "[extensions]\n\tsubmodulePathConfig = true\n",
    )
    .map_err(grit_lib::error::Error::Io)?;
    assert!(submodule_path_config_enabled(&git_dir));
    let modules = submodule_modules_git_dir(&git_dir, "nested/sub");
    assert!(modules.to_string_lossy().contains("modules"));
    validate_submodule_path(wt, "nested/sub")?;
    let _ = submodule_gitdir_outer_conflict(&git_dir, "nested/sub");
    let cfg = ConfigFile::parse(
        git_dir.join("config").as_path(),
        "[submodule \"nested/sub\"]\n\tpath = nested/sub\n\turl = https://example.com/x.git\n",
        ConfigScope::Local,
    )?;
    let default = compute_default_submodule_gitdir(wt, &git_dir, &cfg, "nested/sub")?;
    assert!(!default.is_empty());
    let mut cfg_mut = cfg.clone();
    let written = ensure_submodule_gitdir_config(wt, &git_dir, &mut cfg_mut, "nested/sub")?;
    assert_eq!(written, default);
    let fs_path = submodule_gitdir_filesystem_path(wt, &git_dir, &cfg, "nested/sub")?;
    assert!(fs_path.is_absolute() || fs_path.starts_with(wt));
    Ok(())
}
