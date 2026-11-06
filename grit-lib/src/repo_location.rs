//! Path-oriented view of an open repository's git directory and work tree.

use std::path::{Path, PathBuf};

use crate::repo::{local_config_write_path, Repository};

/// Where a repository lives on disk: its git directory and, when it has one, its work tree.
///
/// Build this from an open [`Repository`] with [`From`]: it copies
/// [`Repository::git_dir`](crate::repo::Repository::git_dir) and
/// [`Repository::work_tree`](crate::repo::Repository::work_tree). Discovery metadata lives on
/// [`DiscoveryInfo`](crate::repo::DiscoveryInfo) via [`Repository::discovery_info`](crate::repo::Repository::discovery_info).
///
/// # Examples
///
/// ```
/// use grit_lib::config::environment::RepositoryOptions;
/// use grit_lib::repo::{InitOptions, Repository, RepositoryLocation};
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
/// let discovered = Repository::discover(Some(dir.path()), &RepositoryOptions::empty())?;
/// let location = RepositoryLocation::from(&discovered);
/// assert!(location.git_dir().ends_with(".git"));
/// assert!(location.local_config_path().ends_with(".git/config"));
/// assert_eq!(location, RepositoryLocation::from(&repo));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLocation {
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
}

impl RepositoryLocation {
    /// Absolute path of the git directory (`.git`, a linked worktree's admin directory, or the
    /// root of a bare repository).
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Absolute path of the work tree, or `None` for a bare repository.
    #[must_use]
    pub fn work_tree(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    /// The repository-local config file that `--local` reads and writes (`.git/config`, or the
    /// common directory's `config` for a linked worktree).
    #[must_use]
    pub fn local_config_path(&self) -> PathBuf {
        local_config_write_path(&self.git_dir)
    }
}

impl From<&Repository> for RepositoryLocation {
    fn from(repo: &Repository) -> Self {
        Self {
            git_dir: repo.git_dir().to_path_buf(),
            work_tree: repo.work_tree().map(Path::to_path_buf),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::{ConfigSet, LoadConfigOptions};
    use crate::environment::{Environment, RepositoryOptions};
    use crate::repo::{InitOptions, RepositoryError};
    use std::ffi::OsString;
    use std::fs;

    fn options_at(cwd: &Path, vars: &[(&str, &str)]) -> RepositoryOptions {
        let vars: Vec<(OsString, OsString)> = vars
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();
        RepositoryOptions {
            environment: Environment::from_vars(vars, cwd.to_path_buf()),
            ..RepositoryOptions::default()
        }
    }

    fn init(dir: &Path) {
        Repository::init(dir, &InitOptions::default()).unwrap();
    }

    fn discover_location(options: &RepositoryOptions) -> RepositoryLocation {
        RepositoryLocation::from(&Repository::discover(None, options).unwrap())
    }

    #[test]
    fn dot_git_layout_from_subdirectory_matches_root_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        let sub = tmp.path().join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let options = options_at(&sub, &[]);

        let location = discover_location(&options);
        assert_eq!(location, discover_location(&options_at(tmp.path(), &[])));
        assert_eq!(
            location.local_config_path(),
            location.git_dir().join("config")
        );
    }

    #[test]
    fn missing_repository_reports_not_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let options = options_at(tmp.path(), &[("GIT_CEILING_DIRECTORIES", "/")]);
        let err = Repository::discover(None, &options).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::Repository(crate::repo::RepositoryError::NotARepository(_))
        ));
    }

    #[test]
    fn unsupported_format_version_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        fs::write(
            tmp.path().join(".git/config"),
            "[core]\n\trepositoryformatversion = 2\n",
        )
        .unwrap();
        let err = Repository::discover(None, &options_at(tmp.path(), &[])).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::Repository(RepositoryError::UnsupportedRepositoryFormatVersion {
                version: 2
            })
        ));
    }

    #[test]
    fn layout_changing_local_config_still_discovers() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        let other = tmp.path().join("elsewhere");
        fs::create_dir(&other).unwrap();
        fs::write(tmp.path().join(".git/extra"), "[core]\n\tbare = true\n").unwrap();
        let config = tmp.path().join(".git/config");
        let original = fs::read_to_string(&config).unwrap();
        for extra in [
            format!("[core]\n\tworktree = {}\n", other.display()),
            "[core]\n\tbare = true\n".to_owned(),
            "[include]\n\tpath = extra\n".to_owned(),
        ] {
            fs::write(&config, format!("{original}{extra}")).unwrap();
            let options = options_at(tmp.path(), &[]);
            let _ = Repository::discover(None, &options).expect("discover {extra}");
        }
    }

    #[test]
    fn layout_changing_override_and_global_config_match_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init(&repo);
        let other = tmp.path().join("elsewhere");
        fs::create_dir(&other).unwrap();
        let global = tmp.path().join("global.cfg");
        fs::write(
            &global,
            format!("[core]\n\tworktree = {}\n", other.display()),
        )
        .unwrap();
        let global = global.to_str().unwrap().to_owned();
        let cases: [Vec<(&str, &str)>; 3] = [
            vec![
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "core.bare"),
                ("GIT_CONFIG_VALUE_0", "true"),
            ],
            vec![("GIT_CONFIG_PARAMETERS", "'core.bare'='true'")],
            vec![("GIT_CONFIG_GLOBAL", global.as_str())],
        ];
        for vars in cases {
            let options = options_at(&repo, &vars);
            let _ = Repository::discover(None, &options).expect("discover {vars:?}");
        }
        let bare_override = options_at(
            &repo,
            &[
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "core.bare"),
                ("GIT_CONFIG_VALUE_0", "true"),
            ],
        );
        assert_eq!(discover_location(&bare_override).work_tree(), None);
    }

    #[test]
    fn discover_loads_merged_config_cascade() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init(&repo);
        let global = tmp.path().join("global.cfg");
        fs::write(&global, "[user]\n\tname = Global\n").unwrap();
        let options = options_at(
            &repo,
            &[
                ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "review.cmdline"),
                ("GIT_CONFIG_VALUE_0", "yes"),
            ],
        );
        let handle = Repository::discover(None, &options).unwrap();
        let location = RepositoryLocation::from(&handle);
        let config = (*handle.config().snapshot().unwrap()).clone();
        let load_opts = LoadConfigOptions::standard(&options.environment, Some(location.git_dir()));
        let expected =
            ConfigSet::load(&options.environment, Some(location.git_dir()), &load_opts).unwrap();
        let keyed = |set: &ConfigSet| -> Vec<(String, Option<String>)> {
            set.entries()
                .iter()
                .map(|e| (e.key.clone(), e.value.clone()))
                .collect()
        };
        assert_eq!(keyed(&config), keyed(&expected));
        assert_eq!(config.get("user.name").unwrap().as_deref(), Some("Global"));
        assert_eq!(
            config.get("review.cmdline").unwrap().as_deref(),
            Some("yes")
        );
    }

    #[test]
    fn environment_overrides_still_discover() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        let git_dir = tmp.path().join(".git");
        let git_dir = git_dir.to_str().unwrap();
        let ceiling = tmp.path().parent().unwrap().to_str().unwrap().to_owned();
        for vars in [
            vec![("GIT_DIR", git_dir)],
            vec![("GIT_CEILING_DIRECTORIES", ceiling.as_str())],
        ] {
            let options = options_at(tmp.path(), &vars);
            let _ = Repository::discover(None, &options).expect("discover {vars:?}");
        }
        let foreign = {
            use crate::ownership_check::ForeignOwnershipCheck;
            use std::sync::Arc;
            options_at(tmp.path(), &[]).with_ownership_check(Arc::new(ForeignOwnershipCheck))
        };
        let err = Repository::discover(None, &foreign).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::Repository(
                crate::repo::RepositoryError::DubiousOwnership { .. }
                    | crate::repo::RepositoryError::NotARepository(_)
            )
        ));
    }

    #[test]
    fn gitfile_and_bare_layouts_discover() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare.git");
        Repository::init(
            &bare,
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let options = options_at(&bare, &[]);
        let location = discover_location(&options);
        assert_eq!(location.work_tree(), None);

        let linked = tmp.path().join("linked");
        fs::create_dir(&linked).unwrap();
        fs::write(linked.join(".git"), format!("gitdir: {}\n", bare.display())).unwrap();
        let options = options_at(&linked, &[]);
        assert_eq!(discover_location(&options).git_dir(), location.git_dir());
    }

    #[test]
    fn location_from_repository_copies_paths() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        let repo = Repository::discover(None, &options_at(tmp.path(), &[])).unwrap();
        let location = RepositoryLocation::from(&repo);
        assert_eq!(location.git_dir(), repo.git_dir());
        assert_eq!(location.work_tree(), repo.work_tree());
    }
}
