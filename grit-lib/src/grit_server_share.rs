//! Shared branches on a grit hosting server, such as grithub.
//!
//! A branch in your space can be shared with other users. Each member works
//! on it in their own space and has their own head: only they move their
//! `refs/heads/<branch>`, and the server keeps every other member's head
//! beside it as `refs/shared/<branch>/<user>`, which nobody pushes to. A push
//! to your own head never needs `--force` on the server, and every head
//! anyone pushes is kept in the share's log.
//!
//! A clone talks to the server about shares with two endpoints:
//!
//! 1. [`share`] — `POST .grit/shared/<owner>/<project>` with a
//!    [`ShareRequest`]: share a branch, add or remove members, make or revoke
//!    a share link, leave, or list the project's shares. Every answer is a
//!    [`ShareReply`].
//! 2. [`accept`] — `POST .grit/share/accept` with a [`ShareAcceptRequest`]:
//!    accept a share link (`https://<server>/share/<token>`), which makes
//!    the user a member and their copy of the project.
//!
//! Both are authenticated like Git requests (the stored token is the
//! basic-auth password), with JSON bodies the server serializes from these
//! same types. A refusal the user can act on (not a member, a used-up link)
//! comes back as a reply with a message, not an HTTP error.

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::grit_server_auth::{not_grit_server_or_http, ServerAuthError, ServerUrl};
use crate::remote::RemoteError;
use crate::transport::http::HttpClient;

/// Path, under the server root, of the share endpoint: `POST
/// .grit/shared/<owner>/<project>` with a [`ShareRequest`] answers a
/// [`ShareReply`].
pub(crate) const SHARED_PATH: &str = ".grit/shared";

/// Path, under the server root, of the accept endpoint: `POST
/// .grit/share/accept` with a [`ShareAcceptRequest`] answers a
/// [`ShareAcceptReply`].
pub(crate) const ACCEPT_PATH: &str = ".grit/share/accept";

/// Path prefix, under the server root, of share links: `share/<token>`.
pub(crate) const LINK_PATH: &str = "share";

/// Where other members' heads live in a member's copy:
/// `refs/shared/<branch>/<user>`.
pub const SHARED_REFS: &str = "refs/shared/";

const JSON_CONTENT_TYPE: &str = "application/json";

/// The ref holding `user`'s head of the shared `branch` in another member's
/// copy.
#[must_use]
#[allow(dead_code)]
pub(crate) fn shared_ref(branch: &str, user: &str) -> String {
    format!("{SHARED_REFS}{branch}/{user}")
}

/// The branch and member a `refs/shared/<branch>/<user>` ref (or the part
/// after any prefix, such as a remote-tracking `refs/remotes/<r>/shared/`)
/// names. The member is the last segment; branch names may hold `/`.
#[must_use]
#[allow(dead_code)]
pub(crate) fn parse_shared_ref(rest: &str) -> Option<(&str, &str)> {
    let (branch, user) = rest.rsplit_once('/')?;
    (!branch.is_empty() && !user.is_empty()).then_some((branch, user))
}

/// How many uses a new share link has: one by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkUses {
    /// Accepted at most this many times.
    Count(u32),
    /// Accepted any number of times until it expires or is revoked.
    Unlimited,
}

impl Default for LinkUses {
    fn default() -> Self {
        Self::Count(1)
    }
}

/// The default number of days a share link lasts.
pub const DEFAULT_LINK_DAYS: u32 = 7;

/// The most days a share link may last.
pub const MAX_LINK_DAYS: u32 = 30;

fn default_link_days() -> u32 {
    DEFAULT_LINK_DAYS
}

/// What a new share link allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkOptions {
    /// How many times it can be accepted.
    #[serde(default)]
    pub uses: LinkUses,
    /// How many days it lasts, at most [`MAX_LINK_DAYS`].
    #[serde(default = "default_link_days")]
    pub expires_days: u32,
}

impl Default for LinkOptions {
    fn default() -> Self {
        Self {
            uses: LinkUses::default(),
            expires_days: DEFAULT_LINK_DAYS,
        }
    }
}

/// What to do with a project's shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ShareRequest {
    /// The project's shared branches, or just `branch`.
    List {
        /// Only this branch's share.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    /// Share `branch` of the caller's space with `users` (sharing it first
    /// when it isn't shared yet), and make a share link when `link` is set.
    /// Any member may add people.
    Share {
        /// The branch, in the caller's space.
        branch: String,
        /// People to add: user names, or email addresses. An address that
        /// belongs to an account adds that account; any other is emailed an
        /// invitation to sign up and join.
        #[serde(default)]
        users: Vec<String>,
        /// Make a share link with these options.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<LinkOptions>,
        /// Give the people added (and the link) a snapshot share: the
        /// branch's history without the rest of the project's, cut down to
        /// these paths. A member with a snapshot share can only add people
        /// with the same one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ShareView>,
    },
    /// Remove `users` from the share of `branch`, or end the share when
    /// `users` is empty. The sharer, or someone who can integrate, may
    /// remove members; nobody can remove the sharer. Nobody loses their
    /// head: it stays a branch in their own space.
    Unshare {
        /// The shared branch.
        branch: String,
        /// User names to remove, or email addresses whose invitations to
        /// withdraw; none ends the share.
        #[serde(default)]
        users: Vec<String>,
    },
    /// Leave the share of `branch`. Your head stays a branch in your space.
    Leave {
        /// The shared branch.
        branch: String,
    },
    /// Share `branch` again, after it was integrated, with everyone who was
    /// on it then. The caller's head of `branch` is the new share's start.
    Reshare {
        /// The branch that was shared.
        branch: String,
    },
    /// Revoke the share link `link` (its id) of `branch`.
    RevokeLink {
        /// The shared branch.
        branch: String,
        /// The link's id, from [`ShareLinkInfo::id`].
        link: String,
    },
}

/// Whether a share is still going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareState {
    /// Members are working on it.
    Active,
    /// It was integrated, and the branch deleted from every member's copy.
    Integrated,
    /// The sharer ended it.
    Ended,
}

/// What a member of a share gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareMode {
    /// A copy with the branch's whole history.
    Full,
    /// A snapshot checkout: the branch's commits on a snapshot of its base,
    /// perhaps cut down to some paths.
    Snapshot,
}

/// What a member of a snapshot share sees: the branch's own commits on a
/// snapshot of the base branch, without its history, cut down to `paths`
/// (none: the whole tree). Their pushes are grafted back onto the real
/// history as their head.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareView {
    /// The paths, such as `src/rpc`; empty for the whole tree.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// One member of a share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareMember {
    /// Their user name.
    pub user: String,
    /// Whether they shared the branch first.
    #[serde(default)]
    pub sharer: bool,
    /// Whether this is the caller.
    #[serde(default)]
    pub you: bool,
    /// The tip of their head, once they have a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Their copy, `<user>/<project>`, once they have one: a member added
    /// by name gets it the first time they fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// For a member with a snapshot share: what they see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ShareView>,
}

/// An open share link, without its secret part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLinkInfo {
    /// The link's id, for revoking it.
    pub id: String,
    /// Uses left, or `None` when it has no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses_left: Option<u32>,
    /// When it stops working, RFC 3339.
    pub expires_at: String,
    /// The one user who may accept it, for an invitation by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_user: Option<String>,
    /// For a snapshot share link: what accepting it gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ShareView>,
}

/// A shared branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedBranch {
    /// The branch's name, the same in every member's copy.
    pub branch: String,
    /// The project it was shared from, `<owner>/<project>`.
    pub origin: String,
    /// Who shared it first.
    pub sharer: String,
    /// Whether it is still going.
    pub state: ShareState,
    /// What members get.
    pub mode: ShareMode,
    /// For a snapshot share of some paths: the paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// The read-only branch members' copies follow for it (the upstream's
    /// main for a copy of an organization's project, else the sharer's
    /// default branch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Everyone on it, the sharer first.
    pub members: Vec<ShareMember>,
    /// Email addresses invited who haven't accepted yet (people without an
    /// account when they were invited), for members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invited: Vec<String>,
    /// Open share links, for members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ShareLinkInfo>,
    /// How it was integrated, for [`ShareState::Integrated`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrated: Option<ShareIntegration>,
    /// Its web page.
    pub url: String,
}

/// How a shared branch was integrated. The branch was then deleted from
/// every member's copy; the share is kept for [`INTEGRATED_KEEP_DAYS`] so a
/// later push of the same name can be recognised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareIntegration {
    /// Who integrated it.
    pub by: String,
    /// When, RFC 3339.
    pub at: String,
    /// The project it was merged into, `<owner>/<project>`.
    pub into: String,
    /// The branch it was merged into.
    pub target_branch: String,
    /// The target's tip afterwards (the merge commit, when one was made).
    pub commit: String,
    /// The keys of the commits integrated (change ids, or `patch:<id>`;
    /// see [`crate::divergence`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    /// The pull request it went through, for an organization's project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull: Option<u64>,
    /// The pull request's page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// How many days an integrated share is remembered, with its head log.
#[allow(dead_code)]
pub(crate) const INTEGRATED_KEEP_DAYS: u32 = 14;

/// A share link just made: the only time its URL is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewShareLink {
    /// The URL to send, `https://<server>/share/<token>`.
    pub url: String,
    /// Its id, uses and expiry.
    #[serde(flatten)]
    pub info: ShareLinkInfo,
}

/// The answer to a [`ShareRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareReply {
    /// The caller's project the request was about, `<user>/<project>`.
    pub project: String,
    /// The shares asked about, as they are now.
    #[serde(default)]
    pub shares: Vec<SharedBranch>,
    /// The link a `share` request with `link` made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<NewShareLink>,
    /// Users the request added, who were notified, and email addresses it
    /// invited.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// Users the request removed, and invitations it withdrew.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Why nothing changed, when the request was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

/// The body of an accept request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAcceptRequest {
    /// The share link, or just its token.
    pub link: String,
}

/// What accepting a share link did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareAcceptStatus {
    /// The caller is a member now.
    Accepted,
    /// The caller was a member already; nothing changed.
    AlreadyMember,
    /// Nothing changed: see [`ShareAcceptReply::message`].
    Refused,
}

/// The answer to an accept request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAcceptReply {
    /// What happened.
    pub status: ShareAcceptStatus,
    /// The shared branch, unless refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<SharedBranch>,
    /// The caller's copy of the project, `<user>/<project>`, unless refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Its clone URL, unless refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Why it was refused: used up, expired, revoked, or for someone else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The server and token a share link names, such as
/// `https://grithub.dev/share/7Kq2…`.
///
/// `None` when the URL is not `http(s)` or its path is not `share/<token>`.
#[must_use]
pub fn share_link(url: &str) -> Option<(ServerUrl, String)> {
    let server = ServerUrl::parse(url).ok()?;
    let parsed = url::Url::parse(url.trim()).ok()?;
    let path = parsed.path().trim_matches('/');
    let token = path.strip_prefix(LINK_PATH)?.strip_prefix('/')?;
    let token_ok = !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    token_ok.then(|| (server, token.to_owned()))
}

/// Send `request` about the signed-in user's `project` (`<owner>/<project>`)
/// to `server`.
///
/// # Errors
///
/// [`ServerAuthError::TokenRejected`] without a valid token,
/// [`ServerAuthError::NotGritServer`] when the server has no such endpoint or
/// the user can't see the project, and [`ServerAuthError::Http`] otherwise.
/// A refusal the user can act on is a reply with
/// [`ShareReply::refused`], not an error.
pub fn share(
    http: &dyn HttpClient,
    server: &ServerUrl,
    project: &str,
    request: &ShareRequest,
) -> std::result::Result<ShareReply, ServerAuthError> {
    let url = format!("{}{SHARED_PATH}/{project}", server.root_url());
    let request = serde_json::to_vec(request)
        .map_err(|e| ServerAuthError::Http(Error::Message(e.to_string())))?;
    let body = post_json(http, server, &url, &request)?;
    serde_json::from_slice(&body).map_err(|_| ServerAuthError::NotGritServer(server.root_url()))
}

/// Accept the share link `request.link` on `server` as the signed-in user.
///
/// # Errors
///
/// As [`share`]. A link that can't be accepted is a
/// [`ShareAcceptStatus::Refused`] reply, not an error.
pub fn accept(
    http: &dyn HttpClient,
    server: &ServerUrl,
    request: &ShareAcceptRequest,
) -> std::result::Result<ShareAcceptReply, ServerAuthError> {
    let url = format!("{}{ACCEPT_PATH}", server.root_url());
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
    fn share_links_name_a_server_and_token() {
        let (server, token) = share_link("https://grithub.dev/share/7Kq2-x_9").unwrap();
        assert_eq!(server.root_url(), "https://grithub.dev/");
        assert_eq!(token, "7Kq2-x_9");
        assert!(share_link("https://grithub.dev/share/").is_none());
        assert!(share_link("https://grithub.dev/shares/abc").is_none());
        assert!(share_link("https://grithub.dev/share/a/b").is_none());
        assert!(share_link("https://grithub.dev/alice/proj").is_none());
    }

    #[test]
    fn shared_refs_split_at_the_last_slash() {
        assert_eq!(shared_ref("feature", "alice"), "refs/shared/feature/alice");
        assert_eq!(
            parse_shared_ref("team/feature/alice"),
            Some(("team/feature", "alice"))
        );
        assert_eq!(parse_shared_ref("alice"), None);
        assert_eq!(parse_shared_ref("feature/"), None);
    }

    #[test]
    fn requests_are_tagged_by_action() {
        let request = ShareRequest::Share {
            branch: "feature".into(),
            users: vec!["bob".into()],
            link: Some(LinkOptions::default()),
            view: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["action"], "share");
        assert_eq!(json["link"]["uses"], serde_json::json!({ "count": 1 }));
        assert_eq!(json["link"]["expires_days"], 7);
        let back: ShareRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);
        let list: ShareRequest = serde_json::from_str(r#"{"action":"list"}"#).unwrap();
        assert_eq!(list, ShareRequest::List { branch: None });
        let unlimited: LinkOptions = serde_json::from_str(r#"{"uses":"unlimited"}"#).unwrap();
        assert_eq!(unlimited.uses, LinkUses::Unlimited);
        assert_eq!(unlimited.expires_days, DEFAULT_LINK_DAYS);
    }
}
