//! Your space on a grit hosting server, such as grithub.
//!
//! On grithub, an organization's projects (`<domain>/<project>`) are read-only:
//! everyone works in a private copy in their own space (`<you>/<project>`) and
//! integrates from there. A clone of either can ask the server two things:
//!
//! 1. [`fetch_space`] — where my work on this project goes: the project in my
//!    space ([`Space`]), made as a copy of the organization's project when I
//!    have none yet, which of its branches follow upstream (and are read-only
//!    there), and whether I may integrate into the target.
//! 2. [`integrate_branch`] — merge a branch of my space into its target: the
//!    upstream project's main for a copy, else the project's own main
//!    ([`IntegrateBranchReply`]).
//!
//! Both are `POST`s authenticated like Git requests (the stored token is the
//! basic-auth password), with JSON bodies the server serializes from these
//! same types. A refusal the user can act on (not a member, conflicts) comes
//! back as a reply with a message, not an HTTP error.

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::grit_server_auth::{not_grit_server_or_http, ServerAuthError, ServerUrl};
use crate::remote::RemoteError;
use crate::transport::http::HttpClient;

/// Path, under the server root, of the space endpoint: `POST
/// .grit/space/<owner>/<project>` answers a [`Space`].
pub(crate) const SPACE_PATH: &str = ".grit/space";

/// Path, under the server root, of the integrate endpoint: `POST
/// .grit/integrate/<owner>/<project>` with an [`IntegrateBranchRequest`]
/// answers an [`IntegrateBranchReply`].
pub(crate) const INTEGRATE_PATH: &str = ".grit/integrate";

const JSON_CONTENT_TYPE: &str = "application/json";

/// Where the signed-in user's work on a project goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    /// The project in the user's space, `<user>/<project>`.
    pub project: String,
    /// Its clone URL.
    pub url: String,
    /// The project it follows, `<owner>/<project>`, for a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The upstream project's clone URL, for a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_url: Option<String>,
    /// The branch work is integrated into: upstream's main branch for a copy,
    /// else the project's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
    /// Branches the space follows from upstream: read-only there, so never
    /// pushed to it.
    #[serde(default)]
    pub followed: Vec<String>,
    /// Branches of the space shared with other users
    /// ([`crate::remote::grit_server_share`]): each member's head is theirs alone, so
    /// a push replaces it without `--force`, and the other members' heads
    /// are fetched from `refs/shared/<branch>/<user>`.
    #[serde(default)]
    pub shared: Vec<String>,
    /// Whether the user may integrate into the target.
    pub can_integrate: bool,
    /// Whether this request made the copy.
    #[serde(default)]
    pub created: bool,
}

/// The body of an integrate request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrateBranchRequest {
    /// The branch of the user's space to merge.
    pub branch: String,
    /// For a shared branch: members whose heads may have changes the
    /// caller's lacks. Those changes stay in their copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub without: Vec<String>,
}

/// A member's head of a shared branch with changes the integrated head
/// lacks: integration waits for it, unless the member is named in
/// [`IntegrateBranchRequest::without`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingHead {
    /// The member.
    pub user: String,
    /// The tip of their head.
    pub head: String,
    /// The subjects of the commits whose changes the caller's head lacks,
    /// oldest first.
    pub commits: Vec<String>,
}

/// What an integrate request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrateBranchStatus {
    /// A merge commit put the branch on the target.
    Merged,
    /// The target had every commit already.
    AlreadyIntegrated,
    /// The target had no such branch yet; it starts at the branch's tip.
    Started,
    /// Nothing changed: see [`IntegrateBranchReply::message`].
    Refused,
}

/// The reply to an integrate request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrateBranchReply {
    /// What happened.
    pub status: IntegrateBranchStatus,
    /// The project merged into, `<owner>/<project>`.
    pub project: String,
    /// The branch merged into.
    pub target_branch: String,
    /// The target branch's tip before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The target branch's tip afterwards (the merge commit, when one was made).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The pull request the merge went through, for a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull: Option<u64>,
    /// A web page about it: the pull request, or where to propose the branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Why it was refused, for [`IntegrateBranchStatus::Refused`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// For a shared branch refused because other members' heads have
    /// changes the caller's lacks: those heads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<WaitingHead>,
    /// For a shared branch that was integrated: the other members, whose
    /// copies no longer have the branch. The share is over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unshared: Vec<String>,
}

/// The server and project (`<owner>/<project>`) a hosted repository URL names,
/// such as `https://grithub.dev/gitbutler.com/grit.git`.
///
/// `None` when the URL is not `http(s)` or its path is not exactly two
/// segments.
#[must_use]
pub fn hosted_project(url: &str) -> Option<(ServerUrl, String)> {
    let server = ServerUrl::parse(url).ok()?;
    let parsed = url::Url::parse(url.trim()).ok()?;
    let path = parsed.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let segment = |s: &str| !s.is_empty() && !s.contains('/') && !s.starts_with('.');
    (segment(owner) && segment(name)).then(|| (server, format!("{owner}/{name}")))
}

/// Ask `server` where the signed-in user's work on `project` goes, making
/// their copy of it when it is an organization's project they have none of.
///
/// # Errors
///
/// [`ServerAuthError::TokenRejected`] without a valid token,
/// [`ServerAuthError::NotGritServer`] when the server has no such endpoint or
/// the user can't see the project, and [`ServerAuthError::Http`] otherwise.
pub fn fetch_space(
    http: &dyn HttpClient,
    server: &ServerUrl,
    project: &str,
) -> std::result::Result<Space, ServerAuthError> {
    let url = format!("{}{SPACE_PATH}/{project}", server.root_url());
    let body = post_json(http, server, &url, b"{}")?;
    serde_json::from_slice(&body).map_err(|_| ServerAuthError::NotGritServer(server.root_url()))
}

/// Ask `server` to merge `request.branch` of the signed-in user's space for
/// `project` into its target.
///
/// # Errors
///
/// As [`fetch_space`]. A refusal the user can act on is an
/// [`IntegrateBranchStatus::Refused`] reply, not an error.
pub fn integrate_branch(
    http: &dyn HttpClient,
    server: &ServerUrl,
    project: &str,
    request: &IntegrateBranchRequest,
) -> std::result::Result<IntegrateBranchReply, ServerAuthError> {
    let url = format!("{}{INTEGRATE_PATH}/{project}", server.root_url());
    let request = serde_json::to_vec(request)
        .map_err(|e| ServerAuthError::Http(Error::Message(e.to_string())))?;
    let body = post_json(http, server, &url, &request)?;
    serde_json::from_slice(&body).map_err(|_| ServerAuthError::NotGritServer(server.root_url()))
}

fn post_json(
    http: &dyn HttpClient,
    server: &ServerUrl,
    url: &str,
    body: &[u8],
) -> std::result::Result<Vec<u8>, ServerAuthError> {
    http.post(url, JSON_CONTENT_TYPE, JSON_CONTENT_TYPE, body, None)
        .map_err(|e| match e {
            RemoteError::Auth { .. } => ServerAuthError::TokenRejected,
            other => not_grit_server_or_http(server, other),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_projects_are_two_segments() {
        let (server, project) =
            hosted_project("https://grithub.dev/gitbutler.com/grit.git").unwrap();
        assert_eq!(server.root_url(), "https://grithub.dev/");
        assert_eq!(project, "gitbutler.com/grit");
        let (server, project) = hosted_project("http://127.0.0.1:8080/alice/proj/").unwrap();
        assert_eq!(server.root_url(), "http://127.0.0.1:8080/");
        assert_eq!(project, "alice/proj");
        assert!(hosted_project("https://grithub.dev/alice").is_none());
        assert!(hosted_project("https://grithub.dev/a/b/c").is_none());
        assert!(hosted_project("git@github.com:a/b.git").is_none());
        assert!(hosted_project("/local/path/a/b").is_none());
    }

    #[test]
    fn replies_round_trip() {
        let reply = IntegrateBranchReply {
            status: IntegrateBranchStatus::AlreadyIntegrated,
            project: "example.com/widget".to_owned(),
            target_branch: "main".to_owned(),
            previous: None,
            head: Some("abc".to_owned()),
            pull: Some(3),
            url: None,
            message: None,
            waiting: Vec::new(),
            unshared: Vec::new(),
        };
        let json = serde_json::to_string(&reply).unwrap();
        assert!(json.contains("\"already_integrated\""), "{json}");
        assert_eq!(
            serde_json::from_str::<IntegrateBranchReply>(&json).unwrap(),
            reply
        );
        let space: Space = serde_json::from_str(
            r#"{"project":"alice/widget","url":"https://h/alice/widget.git","can_integrate":false}"#,
        )
        .unwrap();
        assert!(space.followed.is_empty() && !space.created && space.upstream.is_none());
    }

    /// Answers every POST with `answer`, recording the URL and body.
    struct Canned {
        answer: std::sync::Mutex<Option<crate::error::Result<Vec<u8>>>>,
        seen: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl Canned {
        fn new(answer: crate::error::Result<Vec<u8>>) -> Self {
            Self {
                answer: std::sync::Mutex::new(Some(answer)),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpClient for Canned {
        fn get(
            &self,
            _url: &str,
            _git_protocol: Option<&str>,
        ) -> crate::remote::RemoteResult<Vec<u8>> {
            unreachable!("space requests only POST")
        }

        fn post(
            &self,
            url: &str,
            content_type: &str,
            _accept: &str,
            body: &[u8],
            _git_protocol: Option<&str>,
        ) -> crate::remote::RemoteResult<Vec<u8>> {
            assert_eq!(content_type, JSON_CONTENT_TYPE);
            self.seen
                .lock()
                .unwrap()
                .push((url.to_owned(), body.to_vec()));
            self.answer
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .map_err(RemoteError::from)
        }
    }

    #[test]
    fn spaces_are_asked_for_by_project() {
        let http = Canned::new(Ok(br#"{"project":"alice/widget","url":"https://grithub.dev/alice/widget.git","upstream":"example.com/widget","target_branch":"main","followed":["main"],"can_integrate":true,"created":true}"#.to_vec()));
        let server = ServerUrl::parse("https://grithub.dev").unwrap();
        let space = fetch_space(&http, &server, "example.com/widget").unwrap();
        assert_eq!(space.project, "alice/widget");
        assert_eq!(space.upstream.as_deref(), Some("example.com/widget"));
        assert_eq!(space.followed, ["main"]);
        assert!(space.can_integrate && space.created);
        let seen = http.seen.lock().unwrap();
        assert_eq!(
            seen[0].0,
            "https://grithub.dev/.grit/space/example.com/widget"
        );

        let refused = Canned::new(Err(Error::Remote(RemoteError::Auth {
            url: "https://grithub.dev".to_owned(),
            reason: crate::error::AuthFailure::Rejected,
        })));
        assert!(matches!(
            fetch_space(&refused, &server, "example.com/widget"),
            Err(ServerAuthError::TokenRejected)
        ));
    }

    #[test]
    fn integrating_sends_the_branch() {
        let http = Canned::new(Ok(br#"{"status":"refused","project":"example.com/widget","target_branch":"main","url":"https://grithub.dev/bob/widget/pulls/new/fix","message":"only members integrate"}"#.to_vec()));
        let server = ServerUrl::parse("https://grithub.dev").unwrap();
        let reply = integrate_branch(
            &http,
            &server,
            "bob/widget",
            &IntegrateBranchRequest {
                branch: "fix".to_owned(),
                without: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(reply.status, IntegrateBranchStatus::Refused);
        assert_eq!(reply.message.as_deref(), Some("only members integrate"));
        let seen = http.seen.lock().unwrap();
        assert_eq!(seen[0].0, "https://grithub.dev/.grit/integrate/bob/widget");
        assert_eq!(seen[0].1, br#"{"branch":"fix"}"#);
    }
}
