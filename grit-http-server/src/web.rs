//! The web pages of `grit serve`.
//!
//! Browsers sign in with an email address and password at `/.grit/signin` and
//! get a session cookie ([`SESSION_COOKIE`]). Pages also accept the access
//! token Git uses, so `curl -u user:token` works, but every form needs a
//! session: a form carries a CSRF value derived from the session secret, which
//! a page on another site cannot know.
//!
//! | Page | Who | What |
//! | --- | --- | --- |
//! | `GET /` | signed in | the repositories the user can read |
//! | `GET /<repo>` | read access | branches and recent commits |
//! | `GET`/`POST /.grit/signin` | anyone | sign in |
//! | `POST /.grit/signout` | signed in | end the session |
//! | `GET`/`POST /.grit/password` | signed in | change the password (forced after a temporary one) |
//! | `GET`/`POST /.grit/authorize?code=…` | signed in | approve a `grit auth <url>` sign-in |
//! | `GET`/`POST /.grit/users` | admin | list and add users |
//! | `POST /.grit/grants` | admin | give a user access to a repository |

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Form, Query, State},
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::{Html, IntoResponse, Redirect},
};
use grit_lib::objects::{parse_commit, HashAlgo, ObjectKind};
use grit_lib::porcelain::branch::list_branches;
use grit_lib::remote::grit_server_auth::AUTHORIZE_PATH;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head_for_repository, HeadState};
use grit_protocol::RepositoryOptions;
use time::OffsetDateTime;

use crate::server::{presented_token, store_failure, Authorized, ServerState};
use crate::store::{generate_password, Access, HashedPassword, StoreError, Token, User};

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "grit_session";

/// How long a web session lasts.
const SESSION_LIFETIME: time::Duration = time::Duration::days(30);

/// How many commits the repository page lists.
const RECENT_COMMITS: usize = 20;

/// Path of the sign-in page.
pub const SIGNIN_PATH: &str = "/.grit/signin";
/// Path of the sign-out form target.
pub const SIGNOUT_PATH: &str = "/.grit/signout";
/// Path of the change-password page.
pub const PASSWORD_PATH: &str = "/.grit/password";
/// Path of the admin users page.
pub const USERS_PATH: &str = "/.grit/users";
/// Path of the admin grant form target.
pub const GRANTS_PATH: &str = "/.grit/grants";

/// Who is looking at a page.
pub(crate) struct Viewer {
    user: User,
    /// The session secret when signed in through the browser; `None` for a
    /// request authenticated with an access token.
    session: Option<Token>,
}

impl Viewer {
    /// The CSRF value forms must send back, or `None` without a session.
    fn csrf(&self) -> Option<String> {
        self.session.as_ref().map(csrf_for)
    }
}

/// The CSRF value for session `secret`: a hash only its holder can compute.
fn csrf_for(secret: &Token) -> String {
    hex::encode(
        HashAlgo::Sha256.checksum_digest(format!("grit-csrf:{}", secret.secret()).as_bytes()),
    )
}

/// The session secret in the request's cookies.
fn session_cookie(headers: &HeaderMap) -> Option<Token> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| Token::from_secret(value.trim()))
        .filter(|token| !token.secret().is_empty())
}

/// Percent-encode `value` for a query string.
fn encode_query(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `next` when it is a path on this server, else `/`. Keeps sign-in from
/// redirecting anywhere else.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(path) if path.starts_with('/') && !path.starts_with("//") && !path.contains('\\') => {
            path.to_owned()
        }
        _ => "/".to_owned(),
    }
}

fn redirect_to_signin(next: &str) -> Response<Body> {
    Redirect::to(&format!("{SIGNIN_PATH}?next={}", encode_query(next))).into_response()
}

impl ServerState {
    /// The signed-in user, from the session cookie or an access token.
    fn viewer(&self, headers: &HeaderMap) -> Result<Option<Viewer>, Box<Response<Body>>> {
        let store = self.lock_store();
        if let Some(secret) = session_cookie(headers) {
            match store.session_user(&secret, OffsetDateTime::now_utc()) {
                Ok(Some(user)) => {
                    return Ok(Some(Viewer {
                        user,
                        session: Some(secret),
                    }))
                }
                Ok(None) => {}
                Err(e) => return Err(Box::new(store_failure(&e))),
            }
        }
        let Some(token) = presented_token(headers) else {
            return Ok(None);
        };
        match store.authenticate(&token) {
            Ok(user) => Ok(user.map(|user| Viewer {
                user,
                session: None,
            })),
            Err(e) => Err(Box::new(store_failure(&e))),
        }
    }

    /// The viewer of page `path`: anyone signed in, sent to sign in otherwise,
    /// and to the password page first while their password is temporary.
    pub(crate) fn page_viewer(
        &self,
        headers: &HeaderMap,
        path: &str,
    ) -> Result<Viewer, Box<Response<Body>>> {
        let Some(viewer) = self.viewer(headers)? else {
            return Err(Box::new(redirect_to_signin(path)));
        };
        if viewer.user.must_change_password && viewer.session.is_some() {
            return Err(Box::new(
                Redirect::to(&format!("{PASSWORD_PATH}?next={}", encode_query(path)))
                    .into_response(),
            ));
        }
        Ok(viewer)
    }

    /// The viewer submitting a form: signed in with a session, with the
    /// session's CSRF value in the form.
    fn form_viewer(
        &self,
        headers: &HeaderMap,
        csrf: &str,
        path: &str,
    ) -> Result<Viewer, Box<Response<Body>>> {
        let viewer = match self.viewer(headers)? {
            Some(viewer) if viewer.session.is_some() => viewer,
            _ => return Err(Box::new(redirect_to_signin(path))),
        };
        if viewer.csrf().as_deref() != Some(csrf) {
            return Err(Box::new(
                (StatusCode::FORBIDDEN, "the form expired; reload the page\n").into_response(),
            ));
        }
        Ok(viewer)
    }

    fn admin_viewer(
        &self,
        viewer: Result<Viewer, Box<Response<Body>>>,
    ) -> Result<Viewer, Box<Response<Body>>> {
        let viewer = viewer?;
        if !viewer.user.admin {
            return Err(Box::new(
                (StatusCode::FORBIDDEN, "only admins can manage users\n").into_response(),
            ));
        }
        Ok(viewer)
    }
}

/// Run `work` (password hashing) off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Box<Response<Body>>> {
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        Box::new((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response())
    })
}

/// `http://<Host>` for clone URLs on the pages.
pub(crate) fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
}

#[derive(serde::Deserialize)]
pub(crate) struct NextQuery {
    next: Option<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct SigninForm {
    email: String,
    password: String,
    next: Option<String>,
}

/// `GET /.grit/signin`.
pub(crate) async fn signin_page(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<NextQuery>,
    headers: HeaderMap,
) -> Response<Body> {
    let next = safe_next(query.next.as_deref());
    if let Ok(Some(_)) = state.viewer(&headers) {
        return Redirect::to(&next).into_response();
    }
    signin_form(&next, "", None, StatusCode::OK)
}

fn signin_form(next: &str, email: &str, error: Option<&str>, status: StatusCode) -> Response<Body> {
    let error = error
        .map(|e| format!("<p class=\"error\">{}</p>\n", escape_html(e)))
        .unwrap_or_default();
    let body = format!(
        "<h1>Sign in</h1>\n{error}<form method=\"post\" action=\"{SIGNIN_PATH}\" class=\"stack\">\n\
         <input type=\"hidden\" name=\"next\" value=\"{}\">\n\
         <label>Email <input type=\"email\" name=\"email\" value=\"{}\" autocomplete=\"username\" required autofocus></label>\n\
         <label>Password <input type=\"password\" name=\"password\" autocomplete=\"current-password\" required></label>\n\
         <button type=\"submit\">Sign in</button>\n</form>\n",
        escape_html(next),
        escape_html(email),
    );
    let mut response = page("Sign in", None, &body);
    *response.status_mut() = status;
    response
}

/// `POST /.grit/signin`: check the password and start a session.
pub(crate) async fn signin_submit(
    State(state): State<Arc<ServerState>>,
    Form(form): Form<SigninForm>,
) -> Response<Body> {
    let next = safe_next(form.next.as_deref());
    let found = match state.lock_store().password_of(&form.email) {
        Ok(found) => found,
        Err(e) => return store_failure(&e),
    };
    // Verify against a stand-in hash when there is no such user, so the answer
    // takes as long either way and does not reveal which emails exist.
    let stand_in = state.stand_in_password.clone();
    let password = form.password.clone();
    let verified = blocking(move || match found {
        Some((user, hash)) => hash.verify(&password).then_some(user),
        None => {
            let _ = stand_in.verify(&password);
            None
        }
    })
    .await;
    let user = match verified {
        Ok(Some(user)) => user,
        Ok(None) => {
            return signin_form(
                &next,
                &form.email,
                Some("Wrong email or password."),
                StatusCode::UNAUTHORIZED,
            )
        }
        Err(resp) => return *resp,
    };
    let secret =
        match state
            .lock_store()
            .create_session(&user, OffsetDateTime::now_utc(), SESSION_LIFETIME)
        {
            Ok(secret) => secret,
            Err(e) => return store_failure(&e),
        };
    let target = if user.must_change_password {
        format!("{PASSWORD_PATH}?next={}", encode_query(&next))
    } else {
        next
    };
    with_cookie(
        Redirect::to(&target).into_response(),
        &format!(
            "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
            secret.secret(),
            SESSION_LIFETIME.whole_seconds()
        ),
    )
}

fn with_cookie(mut response: Response<Body>, cookie: &str) -> Response<Body> {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

#[derive(serde::Deserialize)]
pub(crate) struct CsrfForm {
    csrf: String,
}

/// `POST /.grit/signout`.
pub(crate) async fn signout(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response<Body> {
    if let Ok(viewer) = state.form_viewer(&headers, &form.csrf, "/") {
        if let Some(secret) = &viewer.session {
            if let Err(e) = state.lock_store().end_session(secret) {
                return store_failure(&e);
            }
        }
    }
    with_cookie(
        Redirect::to(SIGNIN_PATH).into_response(),
        &format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    )
}

/// `GET /.grit/password`.
pub(crate) async fn password_page(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<NextQuery>,
    headers: HeaderMap,
) -> Response<Body> {
    let next = safe_next(query.next.as_deref());
    let viewer = match state.viewer(&headers) {
        Ok(Some(viewer)) if viewer.session.is_some() => viewer,
        Ok(_) => return redirect_to_signin(PASSWORD_PATH),
        Err(resp) => return *resp,
    };
    password_form(&viewer, &next, None, StatusCode::OK)
}

fn password_form(
    viewer: &Viewer,
    next: &str,
    error: Option<&str>,
    status: StatusCode,
) -> Response<Body> {
    let intro = if viewer.user.must_change_password {
        "<p>Your password was set for you. Choose your own to continue.</p>\n"
    } else {
        ""
    };
    let current = if viewer.user.must_change_password {
        String::new()
    } else {
        "<label>Current password <input type=\"password\" name=\"current\" autocomplete=\"current-password\" required></label>\n".to_owned()
    };
    let error = error
        .map(|e| format!("<p class=\"error\">{}</p>\n", escape_html(e)))
        .unwrap_or_default();
    let body = format!(
        "<h1>Change password</h1>\n{intro}{error}<form method=\"post\" action=\"{PASSWORD_PATH}\" class=\"stack\">\n\
         <input type=\"hidden\" name=\"csrf\" value=\"{}\">\n\
         <input type=\"hidden\" name=\"next\" value=\"{}\">\n{current}\
         <label>New password <input type=\"password\" name=\"new\" autocomplete=\"new-password\" minlength=\"{min}\" required></label>\n\
         <label>Repeat it <input type=\"password\" name=\"confirm\" autocomplete=\"new-password\" minlength=\"{min}\" required></label>\n\
         <button type=\"submit\">Save password</button>\n</form>\n",
        viewer.csrf().unwrap_or_default(),
        escape_html(next),
        min = crate::store::MIN_PASSWORD_LENGTH,
    );
    let mut response = page("Change password", Some(viewer), &body);
    *response.status_mut() = status;
    response
}

#[derive(serde::Deserialize)]
pub(crate) struct PasswordForm {
    csrf: String,
    next: Option<String>,
    current: Option<String>,
    new: String,
    confirm: String,
}

/// `POST /.grit/password`.
pub(crate) async fn password_submit(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response<Body> {
    let next = safe_next(form.next.as_deref());
    let viewer = match state.form_viewer(&headers, &form.csrf, PASSWORD_PATH) {
        Ok(viewer) => viewer,
        Err(resp) => return *resp,
    };
    if form.new != form.confirm {
        return password_form(
            &viewer,
            &next,
            Some("The two passwords differ."),
            StatusCode::BAD_REQUEST,
        );
    }
    let stored = match state.lock_store().password_of(&viewer.user.email) {
        Ok(found) => found.map(|(_, hash)| hash),
        Err(e) => return store_failure(&e),
    };
    let must_change = viewer.user.must_change_password;
    let current = form.current.unwrap_or_default();
    let new = form.new;
    let hashed = blocking(move || {
        let current_ok = must_change || stored.is_some_and(|hash| hash.verify(&current));
        current_ok.then(|| HashedPassword::new(&new))
    })
    .await;
    let hashed = match hashed {
        Ok(Some(Ok(hashed))) => hashed,
        Ok(Some(Err(StoreError::WeakPassword))) => {
            return password_form(
                &viewer,
                &next,
                Some(&StoreError::WeakPassword.to_string()),
                StatusCode::BAD_REQUEST,
            )
        }
        Ok(Some(Err(e))) => return store_failure(&e),
        Ok(None) => {
            return password_form(
                &viewer,
                &next,
                Some("The current password is wrong."),
                StatusCode::UNAUTHORIZED,
            )
        }
        Err(resp) => return *resp,
    };
    if let Err(e) = state
        .lock_store()
        .set_password(&viewer.user, &hashed, false)
    {
        return store_failure(&e);
    }
    Redirect::to(&next).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct AuthorizeQuery {
    code: Option<String>,
}

fn authorize_path(code: &str) -> String {
    format!("/{AUTHORIZE_PATH}?code={}", encode_query(code))
}

/// `GET /.grit/authorize?code=…`: ask the user to approve a command-line sign-in.
pub(crate) async fn authorize_page(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<AuthorizeQuery>,
    headers: HeaderMap,
) -> Response<Body> {
    let code = query.code.unwrap_or_default();
    let path = authorize_path(&code);
    let viewer = match state.page_viewer(&headers, &path) {
        Ok(viewer) if viewer.session.is_some() => viewer,
        Ok(_) => return redirect_to_signin(&path),
        Err(resp) => return *resp,
    };
    if !state.lock_logins().is_waiting(&code, Instant::now()) {
        return page(
            "Sign-in expired",
            Some(&viewer),
            "<h1>This sign-in has expired</h1>\n\
             <p>Run <code>grit auth</code> again to get a new code.</p>\n",
        );
    }
    let body = format!(
        "<h1>Sign in on the command line?</h1>\n\
         <p><code>grit</code> is asking to sign in as <strong>{email}</strong>. \
         Approve only if your terminal shows this code:</p>\n\
         <p class=\"code\">{code}</p>\n\
         <form method=\"post\" action=\"/{AUTHORIZE_PATH}\" class=\"row\">\n\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\n\
         <input type=\"hidden\" name=\"code\" value=\"{code}\">\n\
         <button type=\"submit\" name=\"decision\" value=\"approve\">Approve</button>\n\
         <button type=\"submit\" name=\"decision\" value=\"deny\" class=\"secondary\">Deny</button>\n\
         </form>\n",
        email = escape_html(&viewer.user.email),
        code = escape_html(&code.trim().to_ascii_uppercase()),
        csrf = viewer.csrf().unwrap_or_default(),
    );
    page("Approve sign-in", Some(&viewer), &body)
}

#[derive(serde::Deserialize)]
pub(crate) struct AuthorizeForm {
    csrf: String,
    code: String,
    decision: String,
}

/// `POST /.grit/authorize`: record the user's decision.
pub(crate) async fn authorize_submit(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Form(form): Form<AuthorizeForm>,
) -> Response<Body> {
    let viewer = match state.form_viewer(&headers, &form.csrf, &authorize_path(&form.code)) {
        Ok(viewer) => viewer,
        Err(resp) => return *resp,
    };
    let now = Instant::now();
    let approved = form.decision == "approve";
    let decided = {
        let mut logins = state.lock_logins();
        if approved {
            logins.approve(&form.code, viewer.user.clone(), now)
        } else {
            logins.deny(&form.code, now)
        }
    };
    let body = match (decided, approved) {
        (false, _) => {
            "<h1>This sign-in has expired</h1>\n<p>Run <code>grit auth</code> again to get a new code.</p>\n"
        }
        (true, true) => {
            "<h1>Signed in</h1>\n<p>You can close this tab and go back to your terminal.</p>\n"
        }
        (true, false) => "<h1>Sign-in declined</h1>\n<p>The command line was not signed in.</p>\n",
    };
    page("Sign-in", Some(&viewer), body)
}

/// `GET /.grit/users` (admins).
pub(crate) async fn users_page(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response<Body> {
    let viewer = match state.admin_viewer(state.page_viewer(&headers, USERS_PATH)) {
        Ok(viewer) => viewer,
        Err(resp) => return *resp,
    };
    users_listing(&state, &viewer, "")
}

fn users_listing(state: &ServerState, viewer: &Viewer, notice: &str) -> Response<Body> {
    let users = match state.lock_store().users() {
        Ok(users) => users,
        Err(e) => return store_failure(&e),
    };
    let mut rows = String::new();
    let mut options = String::new();
    for summary in &users {
        let email = escape_html(&summary.user.email);
        let access = if summary.user.admin {
            "all repositories (admin)".to_owned()
        } else if summary.grants.is_empty() {
            "no repositories".to_owned()
        } else {
            summary
                .grants
                .iter()
                .map(|g| format!("{} ({})", escape_html(&g.repository), g.access.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let status = if summary.user.must_change_password {
            " <span class=\"badge\">temporary password</span>"
        } else {
            ""
        };
        rows.push_str(&format!(
            "<tr><td>{email}{status}</td><td>{}</td><td>{access}</td></tr>\n",
            summary.tokens
        ));
        if !summary.user.admin {
            options.push_str(&format!("<option value=\"{email}\">{email}</option>\n"));
        }
    }
    let repositories: String = state
        .repositories
        .list()
        .iter()
        .map(|r| {
            let name = escape_html(&r.name);
            format!("<option value=\"{name}\">{name}</option>\n")
        })
        .collect();
    let csrf = viewer.csrf().unwrap_or_default();
    let grant_form = if options.is_empty() || repositories.is_empty() {
        String::new()
    } else {
        format!(
            "<h2>Give access</h2>\n<form method=\"post\" action=\"{GRANTS_PATH}\" class=\"row\">\n\
             <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\n\
             <select name=\"email\" aria-label=\"User\">\n{options}</select>\n\
             <select name=\"repository\" aria-label=\"Repository\">\n{repositories}</select>\n\
             <select name=\"access\" aria-label=\"Access\"><option value=\"read\">read</option><option value=\"write\">write</option></select>\n\
             <button type=\"submit\">Grant</button>\n</form>\n"
        )
    };
    let body = format!(
        "<h1>Users</h1>\n{notice}<table>\n<tr><th>Email</th><th>Tokens</th><th>Access</th></tr>\n{rows}</table>\n\
         <h2>Add a user</h2>\n\
         <p class=\"muted\">They get a temporary password to send them, and choose their own when they first sign in.</p>\n\
         <form method=\"post\" action=\"{USERS_PATH}\" class=\"row\">\n\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\n\
         <input type=\"email\" name=\"email\" placeholder=\"name@example.com\" aria-label=\"Email\" required>\n\
         <label class=\"check\"><input type=\"checkbox\" name=\"admin\" value=\"1\"> admin</label>\n\
         <button type=\"submit\">Add user</button>\n</form>\n{grant_form}"
    );
    page("Users", Some(viewer), &body)
}

#[derive(serde::Deserialize)]
pub(crate) struct AddUserForm {
    csrf: String,
    email: String,
    admin: Option<String>,
}

/// `POST /.grit/users` (admins): add a user with a temporary password.
pub(crate) async fn add_user(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Form(form): Form<AddUserForm>,
) -> Response<Body> {
    let viewer = match state.admin_viewer(state.form_viewer(&headers, &form.csrf, USERS_PATH)) {
        Ok(viewer) => viewer,
        Err(resp) => return *resp,
    };
    let generated = blocking(|| {
        let password = generate_password()?;
        let hash = HashedPassword::new(&password)?;
        Ok::<_, StoreError>((password, hash))
    })
    .await;
    let (password, hash) = match generated {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return store_failure(&e),
        Err(resp) => return *resp,
    };
    let admin = form.admin.is_some();
    let added = state.lock_store().add_user(&form.email, admin, &hash, true);
    let notice = match added {
        Ok(user) => format!(
            "<div class=\"notice\">\n<p>Added <strong>{email}</strong>{role}. Send them these, then they can sign in at \
             <code>{signin}</code>:</p>\n<pre>Email:    {email}\nPassword: {password}</pre>\n\
             <p class=\"muted\">The password is shown only now. They will choose their own when they first sign in.</p>\n</div>\n",
            email = escape_html(&user.email),
            role = if user.admin { " as an admin" } else { "" },
            signin = escape_html(&format!("{}{SIGNIN_PATH}", base_url(&headers))),
            password = escape_html(&password),
        ),
        Err(e @ (StoreError::InvalidEmail(_) | StoreError::UserExists(_))) => {
            format!("<p class=\"error\">{}</p>\n", escape_html(&e.to_string()))
        }
        Err(e) => return store_failure(&e),
    };
    users_listing(&state, &viewer, &notice)
}

#[derive(serde::Deserialize)]
pub(crate) struct GrantForm {
    csrf: String,
    email: String,
    repository: String,
    access: String,
}

/// `POST /.grit/grants` (admins).
pub(crate) async fn grant(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Form(form): Form<GrantForm>,
) -> Response<Body> {
    let viewer = match state.admin_viewer(state.form_viewer(&headers, &form.csrf, USERS_PATH)) {
        Ok(viewer) => viewer,
        Err(resp) => return *resp,
    };
    let access = if form.access == "write" {
        Access::Write
    } else {
        Access::Read
    };
    let result = {
        let mut store = state.lock_store();
        store.require_user(&form.email).and_then(|user| {
            store
                .grant(&user, &form.repository, access)
                .map(|g| (user, g))
        })
    };
    let notice = match result {
        Ok((user, grant)) => format!(
            "<p class=\"notice\">{} now has {} access to {}.</p>\n",
            escape_html(&user.email),
            grant.access.as_str(),
            escape_html(&grant.repository)
        ),
        Err(e @ (StoreError::UnknownUser(_) | StoreError::InvalidRepositoryName(_))) => {
            format!("<p class=\"error\">{}</p>\n", escape_html(&e.to_string()))
        }
        Err(e) => return store_failure(&e),
    };
    users_listing(&state, &viewer, &notice)
}

/// `GET /`: the repositories the viewer can read.
pub(crate) async fn index(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response<Body> {
    let viewer = match state.page_viewer(&headers, "/") {
        Ok(viewer) => viewer,
        Err(resp) => return *resp,
    };
    let base = base_url(&headers);
    let mut rows = String::new();
    {
        let store = state.lock_store();
        for repo in state.repositories.list() {
            let access = match store.access(&viewer.user, &repo.name) {
                Ok(Some(access)) => access,
                Ok(None) => continue,
                Err(e) => return store_failure(&e),
            };
            let name = escape_html(&repo.name);
            rows.push_str(&format!(
                "<tr><td><a href=\"/{name}\">{name}</a></td><td><span class=\"badge\">{}</span></td>\
                 <td><code>git clone {}/{name}.git</code></td></tr>\n",
                access.as_str(),
                escape_html(&base),
            ));
        }
    }
    if rows.is_empty() {
        rows.push_str("<tr><td colspan=\"3\">No repositories you can read.</td></tr>\n");
    }
    page(
        "Repositories",
        Some(&viewer),
        &format!(
            "<h1>Repositories</h1>\n<table>\n<tr><th>Name</th><th>Access</th><th>Clone</th></tr>\n{rows}</table>\n\
             <p class=\"muted\">To clone and push, sign in on the command line with <code>grit auth {}</code>.</p>\n",
            escape_html(&base)
        ),
    )
}

/// What the repository page shows, read off the request thread.
struct RepositorySummary {
    head: Option<String>,
    branches: Vec<(String, String, bool)>,
    commits: Vec<CommitLine>,
}

struct CommitLine {
    oid: String,
    subject: String,
    author: String,
    date: String,
}

/// `GET /<repo>`: branches and recent commits.
pub(crate) async fn repository_page(
    state: &ServerState,
    name: &str,
    headers: &HeaderMap,
) -> Response<Body> {
    let path = format!("/{name}");
    let viewer = match state.page_viewer(headers, &path) {
        Ok(viewer) => viewer,
        Err(resp) => return *resp,
    };
    let Authorized { repo, access, .. } =
        match state.authorize_user(viewer.user.clone(), name, Access::Read) {
            Ok(found) => found,
            Err(resp) => return *resp,
        };
    let repo_path = repo.path.clone();
    let options = state.options.clone();
    let summary = match tokio::task::spawn_blocking(move || summarize(&repo_path, &options)).await {
        Ok(Ok(summary)) => summary,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot read repository: {e}\n"),
            )
                .into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
    };

    let title = escape_html(&repo.name);
    let mut body = format!(
        "<p><a href=\"/\">Repositories</a></p>\n<h1>{title} <span class=\"badge\">{}</span></h1>\n\
         <pre>git clone {}/{title}.git</pre>\n",
        access.as_str(),
        escape_html(&base_url(headers)),
    );
    body.push_str("<h2>Branches</h2>\n");
    if summary.branches.is_empty() {
        body.push_str("<p>No branches yet. Push one to get started.</p>\n");
    } else {
        body.push_str("<table>\n");
        for (branch, oid, current) in &summary.branches {
            let marker = if *current {
                " <span class=\"badge\">HEAD</span>"
            } else {
                ""
            };
            body.push_str(&format!(
                "<tr><td>{}{marker}</td><td><code>{oid}</code></td></tr>\n",
                escape_html(branch)
            ));
        }
        body.push_str("</table>\n");
    }
    if let Some(head) = &summary.head {
        body.push_str(&format!(
            "<h2>Recent commits on {}</h2>\n",
            escape_html(head)
        ));
        body.push_str("<table>\n");
        for commit in &summary.commits {
            body.push_str(&format!(
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                commit.oid,
                escape_html(&commit.subject),
                escape_html(&commit.author),
                commit.date,
            ));
        }
        body.push_str("</table>\n");
    }
    page(&repo.name, Some(&viewer), &body)
}

fn short_hex(oid: &grit_lib::objects::ObjectId) -> String {
    let hex = oid.to_hex();
    hex.get(..7).unwrap_or(&hex).to_owned()
}

/// Read the branches and the first-parent history of `HEAD`.
fn summarize(
    path: &Path,
    options: &RepositoryOptions,
) -> grit_lib::error::Result<RepositorySummary> {
    let repo = Repository::open(path, &options.clone().for_serving())?;
    let branches = list_branches(&repo)?
        .branches
        .into_iter()
        .map(|b| (b.name, short_hex(&b.oid), b.current))
        .collect();
    let (head, mut next) = match resolve_head_for_repository(&repo)? {
        HeadState::Branch {
            short_name, oid, ..
        } => (Some(short_name), oid),
        HeadState::Detached { oid } => (Some("HEAD".to_owned()), Some(oid)),
        HeadState::Invalid => (None, None),
    };
    let mut commits = Vec::new();
    while let Some(oid) = next.take() {
        if commits.len() == RECENT_COMMITS {
            break;
        }
        let object = repo.odb().read(&oid)?;
        if object.kind != ObjectKind::Commit {
            break;
        }
        let commit = parse_commit(&object.data)?;
        let (author, date) = split_signature(&commit.author);
        commits.push(CommitLine {
            oid: short_hex(&oid),
            subject: commit.message.lines().next().unwrap_or_default().to_owned(),
            author,
            date,
        });
        next = commit.parents.first().copied();
    }
    Ok(RepositorySummary {
        head: head.filter(|_| !commits.is_empty()),
        branches,
        commits,
    })
}

/// Split `Name <email> 1712345678 +0200` into the name and a `YYYY-MM-DD` date.
fn split_signature(signature: &str) -> (String, String) {
    let (name, rest) = signature.split_once(" <").unwrap_or((signature, ""));
    let date = rest
        .rsplit_once("> ")
        .and_then(|(_, when)| when.split_whitespace().next())
        .and_then(|seconds| seconds.parse::<i64>().ok())
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .map(|when| when.date().to_string())
        .unwrap_or_default();
    (name.to_owned(), date)
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = "
:root { color-scheme: light dark; --fg: #1f2328; --muted: #59636e; --line: #d1d9e0; --bg: #ffffff; --chip: #eef1f4; --accent: #1f6feb; --bad: #cf222e; }
@media (prefers-color-scheme: dark) { :root { --fg: #e6edf3; --muted: #9198a1; --line: #3d444d; --bg: #0d1117; --chip: #212830; --accent: #4493f8; --bad: #f85149; } }
body { font: 15px/1.5 system-ui, sans-serif; color: var(--fg); background: var(--bg); max-width: 960px; margin: 0 auto; padding: 16px; }
header { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; color: var(--muted); border-bottom: 1px solid var(--line); padding-bottom: 8px; }
header nav { display: flex; align-items: center; gap: 12px; }
header form { margin: 0; }
a { color: inherit; }
table { border-collapse: collapse; width: 100%; }
td, th { text-align: left; padding: 6px 8px; border-bottom: 1px solid var(--line); vertical-align: top; }
pre, code { font: 13px ui-monospace, monospace; }
pre { background: var(--chip); padding: 8px 12px; overflow-x: auto; }
.badge { font-size: 12px; background: var(--chip); border-radius: 10px; padding: 1px 8px; color: var(--muted); }
.muted { color: var(--muted); }
.error { color: var(--bad); }
.notice { border: 1px solid var(--line); border-radius: 6px; padding: 4px 12px; }
.code { font: 600 28px ui-monospace, monospace; letter-spacing: 2px; }
.stack { display: grid; gap: 12px; max-width: 360px; }
.stack label { display: grid; gap: 4px; }
.row { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
input, select, button { font: inherit; padding: 6px 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--bg); color: var(--fg); }
button { background: var(--accent); border-color: var(--accent); color: #fff; cursor: pointer; }
button.secondary, header button { background: var(--chip); border-color: var(--line); color: var(--fg); }
header button { padding: 2px 10px; }
.check { display: flex; gap: 4px; align-items: center; }
";

fn page(title: &str, viewer: Option<&Viewer>, body: &str) -> Response<Body> {
    let nav = viewer
        .map(|viewer| {
            let role = if viewer.user.admin { " (admin)" } else { "" };
            let users = if viewer.user.admin {
                format!("<a href=\"{USERS_PATH}\">Users</a>")
            } else {
                String::new()
            };
            let session = viewer
                .csrf()
                .map(|csrf| {
                    format!(
                        "<a href=\"{PASSWORD_PATH}\">Password</a>\
                         <form method=\"post\" action=\"{SIGNOUT_PATH}\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                         <button type=\"submit\">Sign out</button></form>"
                    )
                })
                .unwrap_or_default();
            format!(
                "<nav><span>{}{role}</span>{users}{session}</nav>",
                escape_html(&viewer.user.email)
            )
        })
        .unwrap_or_default();
    Html(format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{} · grit</title>\n<style>{STYLE}</style>\n</head>\n<body>\n\
         <header><strong><a href=\"/\">grit serve</a></strong>{nav}</header>\n{body}</body>\n</html>\n",
        escape_html(title),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_split_into_name_and_date() {
        assert_eq!(
            split_signature("A U Thor <a@example.com> 1112911993 +0200"),
            ("A U Thor".to_owned(), "2005-04-07".to_owned())
        );
        assert_eq!(split_signature("odd"), ("odd".to_owned(), String::new()));
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(
            escape_html("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn only_local_paths_are_followed_after_sign_in() {
        assert_eq!(safe_next(Some("/app")), "/app");
        assert_eq!(
            safe_next(Some("/.grit/authorize?code=AB")),
            "/.grit/authorize?code=AB"
        );
        for bad in [
            None,
            Some("https://evil.example"),
            Some("//evil.example"),
            Some("/\\evil"),
        ] {
            assert_eq!(safe_next(bad), "/", "{bad:?}");
        }
        assert_eq!(
            encode_query("/.grit/authorize?code=AB-CD"),
            "/.grit/authorize%3Fcode%3DAB-CD"
        );
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; grit_session=grit_abc; other=1"),
        );
        assert_eq!(
            session_cookie(&headers).map(|t| t.secret().to_owned()),
            Some("grit_abc".to_owned())
        );
        let mut empty = HeaderMap::new();
        empty.insert(header::COOKIE, HeaderValue::from_static("grit_session="));
        assert!(session_cookie(&empty).is_none());
    }

    #[test]
    fn csrf_values_depend_on_the_session() {
        let a = csrf_for(&Token::from_secret("grit_a"));
        assert_eq!(a, csrf_for(&Token::from_secret("grit_a")));
        assert_ne!(a, csrf_for(&Token::from_secret("grit_b")));
        assert!(!a.contains("grit_a"));
    }
}
