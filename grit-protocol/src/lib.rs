//! Git smart protocol handlers for HTTP transport.
//!
//! Runs upload-pack and receive-pack in-process via [`grit_lib::remote::serve`], with
//! no subprocess or external `grit` binary. Every push runs the [`PushGuard`], which
//! keeps pushes off the target branch of a repository with snapshot checkouts.

pub mod receive_pack;
pub mod upload_pack;

use std::io::Read;
use std::path::{Path, PathBuf};

use grit_lib::config::git_path::path_for_error_message;
use grit_lib::config::ConfigSet;

pub use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::Odb;
use grit_lib::remote::serve::{
    self, ProtocolVersion, ReceiveHook, ReceivePolicy, RefCommand, RefUpdateRefusal,
    RefUpdateResult, ServeOptions,
};
use grit_lib::remote::snapshot_checkout::TargetGuard;
use grit_lib::repo::Repository;

/// Errors from smart-protocol handlers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The path does not look like a git repository.
    #[error("not a git repository: {0}")]
    NotARepository(String),
    /// Serving the wire protocol failed.
    #[error(transparent)]
    Serve(#[from] serve::ServeError),
    /// Opening or reading repository state failed.
    #[error(transparent)]
    Repository(#[from] grit_lib::error::Error),
    /// Buffered I/O failed while collecting the response.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// The I/O error from reading the client's request stream, for a failed
    /// [`receive_pack::stateless_rpc_from_reader`] exchange.
    ///
    /// Servers use this to answer a request body that could not be read (truncated,
    /// badly encoded, or rejected by the server's own decoding limits) with a client
    /// error rather than a server error. During receive-pack that is the only source of
    /// stream errors: the response is collected in memory, and repository failures are
    /// reported as other [`serve::ServeError`] variants or inside the status report.
    /// Upload-pack can
    /// also return stream errors while reading repository data, so do not use this to
    /// classify upload-pack failures.
    ///
    /// # Returns
    ///
    /// `Some` with the request read error, or `None` for every other failure.
    #[must_use]
    pub fn request_stream_error(&self) -> Option<&std::io::Error> {
        match self {
            Self::Serve(serve::ServeError::Io(e)) => Some(e),
            _ => None,
        }
    }
}

/// Result alias for protocol handlers.
pub type Result<T> = std::result::Result<T, Error>;

/// Server features beyond plain fetch and push, for the `_with` handlers.
#[derive(Debug, Clone, Default)]
pub struct Extensions {
    /// Bundles offered through the protocol v2 `bundle-uri` command.
    pub bundle_list: Option<grit_lib::remote::bundle_uri::BundleList>,
    /// Resumable push endpoint advertised by receive-pack as `pushBundle=<value>`.
    pub push_bundle: Option<String>,
}

/// The in-process checks every push these handlers serve gets.
///
/// A repository with snapshot checkouts refuses pushes that create, update or
/// delete its target branch ([`TargetGuard`]), and tells the pusher how its
/// work reaches the target instead. A repository without checkouts keeps
/// Git's rules.
#[derive(Debug, Clone, Default)]
pub struct PushGuard {
    target: Option<TargetGuard>,
}

impl PushGuard {
    /// The guard for pushes to `repo`.
    ///
    /// # Parameters
    ///
    /// - `repo`: the repository receiving the push.
    #[must_use]
    pub fn for_repository(repo: &Repository) -> Self {
        Self {
            target: TargetGuard::for_repository(repo),
        }
    }

    /// The branch pushes are kept off, when there is one.
    #[must_use]
    pub fn target(&self) -> Option<&str> {
        self.target.as_ref().map(TargetGuard::target)
    }
}

impl ReceiveHook for PushGuard {
    fn update(&self, command: &RefCommand, odb: &Odb) -> std::result::Result<(), RefUpdateRefusal> {
        self.target
            .as_ref()
            .map_or(Ok(()), |guard| guard.update(command, odb))
    }

    fn message(&self, updates: &[RefUpdateResult]) -> Option<String> {
        let guard = self.target.as_ref()?;
        updates
            .iter()
            .any(|u| u.error.is_some() && guard.guards(&u.refname))
            .then(|| target_push_advice(guard.target()))
    }
}

/// What a pusher of the target branch `target` is told to do instead.
fn target_push_advice(target: &str) -> String {
    format!(
        "{target} is the server's target branch: pushes can't change it.\n\
         Push your work to another branch:\n\
         \x20 grit switch -c <branch>\n\
         \x20 grit push origin <branch>\n\
         then merge it into {target} from a snapshot checkout of this repository:\n\
         \x20 grit integrate <branch>\n\
         (In a snapshot checkout, `grit checkin` sends your branches and `grit integrate` merges them.)\n"
    )
}

/// Build [`RepositoryOptions`] from the current process environment
/// ([`RepositoryOptions::from_process`]).
///
/// Captures `HOME`, `GIT_CONFIG_*`, and the other variables [`ConfigSet::load`]
/// uses for global and system config. Call once at server startup and pass the
/// same options into every handler so receive policy and hide-ref rules match
/// a `grit receive-pack` / `grit upload-pack` subprocess.
#[must_use]
pub fn repository_options_from_process() -> RepositoryOptions {
    RepositoryOptions::from_process()
}

/// Map an explicit smart-HTTP protocol version header to a wire version.
#[must_use]
pub fn protocol_version_from_header(version: Option<u8>) -> ProtocolVersion {
    match version {
        Some(2) => ProtocolVersion::V2,
        Some(1) => ProtocolVersion::V1,
        _ => ProtocolVersion::V0,
    }
}

/// Validate that a path looks like a git repository (bare or non-bare).
///
/// # Errors
///
/// Returns [`Error::NotARepository`] when neither `HEAD` nor `.git/HEAD` exists.
pub fn validate_repo_path(path: &Path) -> Result<PathBuf> {
    if path.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    let dot_git = path.join(".git");
    if dot_git.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    Err(Error::NotARepository(path_for_error_message(path)))
}

pub(crate) fn run_upload_pack(
    repo_path: &Path,
    protocol_version: Option<u8>,
    stateless_rpc: bool,
    advertise_refs: bool,
    input: &[u8],
    repository_options: &RepositoryOptions,
    extensions: &Extensions,
) -> Result<Vec<u8>> {
    let repo = Repository::open(repo_path, &repository_options.clone().for_serving())?;
    let config =
        ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default();
    let hidden_refs = grit_lib::refs::hide_refs::hide_ref_patterns_uploadpack(&config);
    let mut opts = ServeOptions {
        protocol: protocol_version_from_header(protocol_version),
        stateless_rpc,
        advertise_refs,
        agent: format!("grit/{}", env!("CARGO_PKG_VERSION")),
        hidden_refs,
        bundle_list: extensions.bundle_list.clone(),
        push_bundle: None,
        ..ServeOptions::default()
    };
    let mut output = Vec::new();
    let _summary = serve::upload_pack(&repo, &mut &*input, &mut output, &mut opts)?;
    Ok(output)
}

pub(crate) fn run_receive_pack(
    repo_path: &Path,
    stateless_rpc: bool,
    advertise_refs: bool,
    input: &mut dyn Read,
    repository_options: &RepositoryOptions,
    extensions: &Extensions,
) -> Result<Vec<u8>> {
    let repo = Repository::open(repo_path, &repository_options.clone().for_serving())?;
    let config =
        ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default();
    let hidden_refs = grit_lib::refs::hide_refs::hide_ref_patterns_receive(&config);
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc,
        advertise_refs,
        agent: format!("grit/{}", env!("CARGO_PKG_VERSION")),
        hidden_refs,
        bundle_list: None,
        push_bundle: extensions.push_bundle.clone(),
        ..ServeOptions::default()
    };
    let policy = ReceivePolicy::from_config(&config);
    let guard = PushGuard::for_repository(&repo);
    let mut output = Vec::new();
    serve::receive_pack_with_hook(&repo, input, &mut output, &opts, &policy, &guard)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::remote::serve::ProtocolVersion;

    #[test]
    fn pushes_of_the_target_are_told_what_to_do_instead() {
        let result = |refname: &str, error: Option<RefUpdateRefusal>| RefUpdateResult {
            refname: refname.to_owned(),
            old: None,
            new: None,
            error,
        };
        let refused = Some(RefUpdateRefusal::TargetBranch {
            target: "main".to_owned(),
        });
        assert_eq!(
            PushGuard::default().message(&[result("refs/heads/main", refused.clone())]),
            None,
            "a repository without checkouts has no guard"
        );
        let guard = PushGuard {
            target: Some(TargetGuard::new("main")),
        };
        assert_eq!(guard.target(), Some("main"));
        assert_eq!(guard.message(&[result("refs/heads/main", None)]), None);
        assert_eq!(
            guard.message(&[result(
                "refs/heads/topic",
                Some(RefUpdateRefusal::StaleInfo)
            )]),
            None
        );
        let advice = guard
            .message(&[
                result("refs/heads/topic", None),
                result("refs/heads/main", refused),
            ])
            .unwrap_or_default();
        assert!(
            advice.starts_with("main is the server's target branch"),
            "{advice}"
        );
        assert!(advice.contains("grit integrate <branch>"), "{advice}");
        // Refused for Git's own reasons (a checked-out branch), it still explains.
        assert!(guard
            .message(&[result(
                "refs/heads/main",
                Some(RefUpdateRefusal::CurrentBranch)
            )])
            .is_some());
    }

    #[test]
    fn protocol_header_maps_versions() {
        assert_eq!(protocol_version_from_header(None), ProtocolVersion::V0);
        assert_eq!(protocol_version_from_header(Some(2)), ProtocolVersion::V2);
    }
}
