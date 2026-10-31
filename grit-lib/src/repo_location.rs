//! Find a repository's git directory without opening it.
//!
//! [`RepositoryLocation::discover`] answers "which repository is this?" for operations that only
//! touch files inside the git directory, such as reading or rewriting `.git/config`. It skips
//! the object database, the ref store and the configuration cascade that
//! [`Repository::discover`] sets up.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{ConfigFile, ConfigScope};
use crate::environment::RepositoryOptions;
use crate::error::Result;
use crate::git_path::CanonicalizeExt;

use super::{
    ensure_valid_ownership, head_file_present, local_config_write_path, parse_repository_format,
    validate_repository_format_parsed, Repository,
};

/// Where a repository lives on disk: its git directory and, when it has one, its work tree.
///
/// Found by [`Self::discover`], which resolves the same repository as [`Repository::discover`]
/// and applies the same checks: the repository format must be supported, and the work tree and
/// git directory must be owned by the current user or allowed by `safe.directory`.
///
/// # Examples
///
/// ```
/// use grit_lib::config::environment::{Environment, RepositoryOptions};
/// use grit_lib::repo::{InitOptions, Repository, RepositoryLocation};
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// Repository::init(dir.path(), &InitOptions::default())?;
///
/// let mut options = RepositoryOptions::default();
/// options.environment = Environment::from_vars(std::iter::empty(), dir.path().to_path_buf());
/// let location = RepositoryLocation::discover(&options)?;
/// assert!(location.git_dir().ends_with(".git"));
/// assert!(location.local_config_path().ends_with(".git/config"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLocation {
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
}

impl RepositoryLocation {
    /// Locate the repository containing the discovery directory of `options.environment`.
    ///
    /// The common layout (a `.git` directory owned by the current user, found by walking up
    /// from the discovery directory) is resolved by reading only `HEAD`, the `objects`
    /// directory and the repository's own `config` file. Every other layout, including
    /// `GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, gitfiles, linked worktrees, bare
    /// repositories, and `core.bare`, `core.worktree` or `[include]` in the repository config,
    /// is resolved by [`Repository::discover`].
    ///
    /// # Errors
    ///
    /// The same errors as [`Repository::discover`]: [`crate::error::Error::NotARepository`]
    /// when no repository is found, [`crate::error::Error::DubiousOwnership`] when
    /// `safe.directory` rejects it, and repository format errors such as
    /// [`crate::error::Error::UnsupportedRepositoryFormatVersion`].
    pub fn discover(options: &RepositoryOptions) -> Result<Self> {
        if let Some(found) = locate_dot_git_dir(options)? {
            return Ok(found);
        }
        let repo = Repository::discover(None, options)?;
        Ok(Self::from(&repo))
    }

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

/// Resolve the plain `.git` directory layout, or `Ok(None)` to defer to full discovery.
fn locate_dot_git_dir(options: &RepositoryOptions) -> Result<Option<RepositoryLocation>> {
    let env = &options.environment;
    let traces_setup = env
        .git_trace_setup
        .as_deref()
        .is_some_and(|p| !p.is_empty() && p != "0");
    let has_ceilings = env
        .git_ceiling_directories
        .as_deref()
        .is_some_and(|c| !c.is_empty());
    if env.git_dir.is_some()
        || env.git_work_tree.is_some()
        || env.git_object_directory.is_some()
        || has_ceilings
        || traces_setup
        || options.test_assume_different_owner
        || env.test_assume_different_owner()
    {
        return Ok(None);
    }

    let start = env.discovery_cwd();
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        match fs::symlink_metadata(&dot_git) {
            Ok(meta) if meta.is_dir() => return accept_dot_git_dir(options, dir, &dot_git),
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Ok(None),
        }
        // A `HEAD` here may make `dir` a bare repository or a linked worktree's admin directory.
        if fs::symlink_metadata(dir.join("HEAD")).is_ok() {
            return Ok(None);
        }
    }
    Ok(None)
}

fn accept_dot_git_dir(
    options: &RepositoryOptions,
    work_tree: &Path,
    dot_git: &Path,
) -> Result<Option<RepositoryLocation>> {
    if !head_file_present(dot_git)
        || dot_git.join("commondir").exists()
        || !dot_git.join("objects").is_dir()
    {
        return Ok(None);
    }
    let Ok(git_dir) = dot_git.canonicalize_simplified() else {
        return Ok(None);
    };

    let config_path = git_dir.join("config");
    match fs::read_to_string(&config_path) {
        Ok(content) => {
            let Ok(file) = ConfigFile::parse(&config_path, &content, ConfigScope::Local) else {
                return Ok(None);
            };
            if changes_discovered_layout(&file) {
                return Ok(None);
            }
            let Ok(format) = parse_repository_format(&content, &config_path) else {
                return Ok(None);
            };
            validate_repository_format_parsed(&format)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Ok(None),
    }

    ensure_valid_ownership(&options.environment, None, Some(work_tree), &git_dir)?;
    Ok(Some(RepositoryLocation {
        git_dir,
        work_tree: Some(work_tree.to_path_buf()),
    }))
}

/// Whether `file` can move the work tree away from the discovery directory, directly or through
/// an included file, which only [`Repository::discover`] resolves.
fn changes_discovered_layout(file: &ConfigFile) -> bool {
    file.entries.iter().any(|entry| match entry.key.as_str() {
        "core.worktree" => true,
        "core.bare" => !entry
            .value
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("false")),
        key => key.starts_with("include.") || key.starts_with("includeif."),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::environment::Environment;
    use crate::repo::InitOptions;
    use std::ffi::OsString;

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

    fn full(options: &RepositoryOptions) -> RepositoryLocation {
        RepositoryLocation::from(&Repository::discover(None, options).unwrap())
    }

    #[test]
    fn dot_git_layout_matches_full_discovery_from_a_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        let sub = tmp.path().join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let options = options_at(&sub, &[]);

        let fast = locate_dot_git_dir(&options).unwrap().unwrap();
        assert_eq!(fast, full(&options));
        assert_eq!(RepositoryLocation::discover(&options).unwrap(), fast);
        assert_eq!(fast.local_config_path(), fast.git_dir().join("config"));
    }

    #[test]
    fn missing_repository_reports_not_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let options = options_at(tmp.path(), &[("GIT_CEILING_DIRECTORIES", "/")]);
        let err = RepositoryLocation::discover(&options).unwrap_err();
        assert!(matches!(err, crate::error::Error::NotARepository(_)));
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
        let err = RepositoryLocation::discover(&options_at(tmp.path(), &[])).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::UnsupportedRepositoryFormatVersion(2)
        ));
    }

    #[test]
    fn layout_changing_config_defers_to_full_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        let other = tmp.path().join("elsewhere");
        fs::create_dir(&other).unwrap();
        let config = tmp.path().join(".git/config");
        let original = fs::read_to_string(&config).unwrap();
        for extra in [
            format!("\tworktree = {}\n", other.display()),
            "\tbare = true\n".to_owned(),
            "[include]\n\tpath = extra\n".to_owned(),
        ] {
            fs::write(&config, format!("{original}[core]\n{extra}")).unwrap();
            let options = options_at(tmp.path(), &[]);
            assert_eq!(locate_dot_git_dir(&options).unwrap(), None, "{extra}");
            assert_eq!(
                RepositoryLocation::discover(&options).unwrap(),
                full(&options)
            );
        }
    }

    #[test]
    fn environment_overrides_defer_to_full_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        let git_dir = tmp.path().join(".git");
        let git_dir = git_dir.to_str().unwrap();
        let ceiling = tmp.path().parent().unwrap().to_str().unwrap().to_owned();
        for vars in [
            vec![("GIT_DIR", git_dir)],
            vec![("GIT_CEILING_DIRECTORIES", ceiling.as_str())],
            vec![("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")],
        ] {
            let options = options_at(tmp.path(), &vars);
            assert_eq!(locate_dot_git_dir(&options).unwrap(), None, "{vars:?}");
        }
    }

    #[test]
    fn gitfile_and_bare_layouts_use_full_discovery() {
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
        assert_eq!(locate_dot_git_dir(&options).unwrap(), None);
        let location = RepositoryLocation::discover(&options).unwrap();
        assert_eq!(location.work_tree(), None);
        assert_eq!(location, full(&options));

        let linked = tmp.path().join("linked");
        fs::create_dir(&linked).unwrap();
        fs::write(linked.join(".git"), format!("gitdir: {}\n", bare.display())).unwrap();
        let options = options_at(&linked, &[]);
        assert_eq!(locate_dot_git_dir(&options).unwrap(), None);
        assert_eq!(
            RepositoryLocation::discover(&options).unwrap(),
            full(&options)
        );
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
