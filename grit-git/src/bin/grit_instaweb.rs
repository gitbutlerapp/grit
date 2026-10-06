//! HTTP server for `git instaweb` — browse a single repository in the browser.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use grit_lib::instaweb::{
    list_display_refs, load_blob, load_commit, load_summary, load_tree, peel_to_commit,
    recent_commits, RefRow, TreeEntryKind,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use std::str::FromStr;

/// Drop inherited descriptors above stderr so a background server cannot keep the
/// test harness pipe (or other caller handles) open.
#[cfg(unix)]
fn close_inherited_fds() {
    use std::fs;
    if let Ok(dir) = fs::read_dir("/proc/self/fd") {
        let mut fds = Vec::new();
        for entry in dir.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(fd) = name.parse::<i32>() else {
                continue;
            };
            if fd > 2 {
                fds.push(fd);
            }
        }
        for fd in fds {
            let _ = nix::unistd::close(fd);
        }
    }
}

#[derive(Parser)]
#[command(name = "grit-instaweb", about = "Grit instaweb repository browser")]
struct Args {
    /// Git directory (`.git` or bare repo root).
    #[arg(long)]
    git_dir: PathBuf,

    /// Working tree for a non-bare repository (required when `--git-dir` is `.git`).
    #[arg(long)]
    work_tree: Option<PathBuf>,

    /// Address to bind (`host:port`).
    #[arg(long)]
    bind: String,

    /// Write the server PID to this file after bind succeeds.
    #[arg(long)]
    pid_file: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    repo: Arc<Repository>,
}

fn main() -> Result<()> {
    #[cfg(unix)]
    close_inherited_fds();
    tokio_run()
}

#[tokio::main]
async fn tokio_run() -> Result<()> {
    let args = Args::parse();
    let repo = open_repository(&args).context("open repository")?;
    let state = Arc::new(AppState {
        repo: Arc::new(repo),
    });

    let app = Router::new()
        .route("/", get(index_page))
        .route("/commit/{oid}", get(commit_page))
        .route("/tree/{oid}", get(tree_page))
        .route("/blob/{oid}", get(blob_page))
        .with_state(state);

    let addr: SocketAddr = args.bind.parse().context("parse bind address")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    if let Some(path) = &args.pid_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(path, format!("{}", std::process::id()))
            .with_context(|| format!("write pid file {}", path.display()))?;
    }
    axum::serve(listener, app).await?;
    Ok(())
}

fn open_repository(args: &Args) -> Result<Repository> {
    let git_dir = args
        .git_dir
        .canonicalize()
        .context("canonicalize git dir")?;
    if let Some(wt) = &args.work_tree {
        return Repository::open(&git_dir, Some(wt)).context("open repository");
    }
    if git_dir
        .file_name()
        .is_some_and(|n| n == ".git" || n == "git")
    {
        if let Some(parent) = git_dir.parent() {
            return Repository::open(&git_dir, Some(parent)).context("open repository");
        }
    }
    Repository::open(&git_dir, None).context("open repository")
}

async fn index_page(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let summary = load_summary(&state.repo)?;
    let refs = list_display_refs(&state.repo)?;
    let head_commit = summary
        .head_oid
        .as_ref()
        .and_then(|o| peel_to_commit(&state.repo, o).ok());
    let ref_links: Vec<(RefRow, ObjectId)> = refs
        .into_iter()
        .map(|r| {
            let link = peel_to_commit(&state.repo, &r.oid).unwrap_or(r.oid);
            (r, link)
        })
        .collect();
    let commits = recent_commits(&state.repo, 30)?;
    Ok(Html(render_index(
        &summary,
        head_commit.as_ref(),
        &ref_links,
        &commits,
    )))
}

async fn commit_page(
    State(state): State<Arc<AppState>>,
    Path(oid_hex): Path<String>,
) -> Result<Html<String>, AppError> {
    let oid = parse_oid(&oid_hex)?;
    let commit_oid = peel_to_commit(&state.repo, &oid)?;
    let commit = load_commit(&state.repo, &commit_oid)?;
    let tree_rows = load_tree(&state.repo, &commit.data.tree)?;
    Ok(Html(render_commit(&commit, &tree_rows)))
}

async fn tree_page(
    State(state): State<Arc<AppState>>,
    Path(oid_hex): Path<String>,
) -> Result<Html<String>, AppError> {
    let oid = parse_oid(&oid_hex)?;
    let rows = load_tree(&state.repo, &oid)?;
    Ok(Html(render_tree(&oid, &rows)))
}

async fn blob_page(
    State(state): State<Arc<AppState>>,
    Path(oid_hex): Path<String>,
) -> Result<Html<String>, AppError> {
    let oid = parse_oid(&oid_hex)?;
    let blob = load_blob(&state.repo, &oid, 256 * 1024)?;
    Ok(Html(render_blob(&blob)))
}

fn parse_oid(hex: &str) -> Result<ObjectId, AppError> {
    ObjectId::from_str(hex).map_err(|e| AppError::bad_request(format!("invalid object id: {e}")))
}

#[derive(Debug)]
struct AppError(anyhow::Error);

impl AppError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self(anyhow::anyhow!(msg.into()))
    }
}

impl From<anyhow::Error> for AppError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}

impl From<grit_lib::error::Error> for AppError {
    fn from(value: grit_lib::error::Error) -> Self {
        Self(anyhow::Error::msg(value.to_string()))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("error: {}", self.0),
        )
            .into_response()
    }
}

fn page_shell(title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>{title}</title>
<style>
:root {{
  --bg: #0f1419;
  --panel: #1a2332;
  --text: #e7ecf3;
  --muted: #9aa8bc;
  --accent: #6cb6ff;
  --border: #2d3a4f;
  font-family: "Segoe UI", system-ui, sans-serif;
}}
body {{ margin: 0; background: var(--bg); color: var(--text); line-height: 1.5; }}
header {{ padding: 1.25rem 1.5rem; border-bottom: 1px solid var(--border); background: #121820; }}
header a {{ color: var(--accent); text-decoration: none; font-weight: 600; }}
main {{ max-width: 960px; margin: 0 auto; padding: 1.5rem; }}
h1 {{ font-size: 1.5rem; margin: 0 0 0.5rem; }}
h2 {{ font-size: 1.1rem; color: var(--muted); margin: 1.5rem 0 0.5rem; }}
p.desc {{ color: var(--muted); }}
table {{ width: 100%; border-collapse: collapse; background: var(--panel); border-radius: 8px; overflow: hidden; }}
th, td {{ padding: 0.5rem 0.75rem; text-align: left; border-bottom: 1px solid var(--border); }}
th {{ color: var(--muted); font-weight: 600; font-size: 0.85rem; }}
tr:last-child td {{ border-bottom: none; }}
a {{ color: var(--accent); text-decoration: none; }}
a:hover {{ text-decoration: underline; }}
code, pre {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9rem; }}
pre {{ background: var(--panel); padding: 1rem; border-radius: 8px; overflow-x: auto; white-space: pre-wrap; word-break: break-word; }}
.mono {{ font-family: ui-monospace, monospace; color: var(--muted); }}
</style>
</head>
<body>
<header><a href="/">Grit instaweb</a></header>
<main>
{body}
</main>
</body>
</html>"#
    )
}

fn esc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

fn render_index(
    summary: &grit_lib::instaweb::RepoSummary,
    head_commit: Option<&ObjectId>,
    refs: &[(RefRow, ObjectId)],
    commits: &[grit_lib::instaweb::CommitRow],
) -> String {
    let desc = summary
        .description
        .as_deref()
        .map(|d| format!(r#"<p class="desc">{}</p>"#, esc(d)))
        .unwrap_or_default();
    let head = match (&summary.head_ref, head_commit) {
        (Some(r), Some(o)) => format!(
            r#"<p>HEAD: <span class="mono">{refname}</span> → <a href="/commit/{oid}">{abbrev}</a></p>"#,
            refname = esc(r),
            oid = o.to_hex(),
            abbrev = esc(&o.to_hex()[..7.min(o.to_hex().len())])
        ),
        _ => String::new(),
    };
    let mut ref_rows = String::new();
    for (r, link_oid) in refs {
        ref_rows.push_str(&format!(
            "<tr><td class=\"mono\">{}</td><td><a href=\"/commit/{}\">{}</a></td></tr>",
            esc(&r.name),
            link_oid.to_hex(),
            esc(&link_oid.to_hex()[..7.min(link_oid.to_hex().len())])
        ));
    }
    let mut commit_rows = String::new();
    for c in commits {
        commit_rows.push_str(&format!(
            "<tr><td class=\"mono\"><a href=\"/commit/{}\">{}</a></td><td>{}</td><td>{}</td></tr>",
            c.oid.to_hex(),
            esc(&c.abbrev),
            esc(&c.author),
            esc(&c.subject)
        ));
    }
    let body = format!(
        r#"<h1>{name}</h1>
{desc}
{head}
<h2>Branches &amp; tags</h2>
<table><thead><tr><th>Ref</th><th>Object</th></tr></thead><tbody>{ref_rows}</tbody></table>
<h2>Recent commits</h2>
<table><thead><tr><th>Commit</th><th>Author</th><th>Subject</th></tr></thead><tbody>{commit_rows}</tbody></table>"#,
        name = esc(&summary.name),
        ref_rows = ref_rows,
        commit_rows = commit_rows,
    );
    page_shell(&format!("{} · instaweb", summary.name), &body)
}

fn render_commit(
    commit: &grit_lib::instaweb::CommitView,
    tree: &[grit_lib::instaweb::TreeRow],
) -> String {
    let tree_html = render_tree_rows(tree);
    let body = format!(
        r#"<h1>{subject}</h1>
<p class="mono"><a href="/commit/{oid}">{abbrev}</a></p>
<p>{author}<br/>{committer}</p>
<pre>{message}</pre>
<h2>Tree</h2>
<table><thead><tr><th>Mode</th><th>Name</th><th>Object</th></tr></thead><tbody>{tree_html}</tbody></table>"#,
        subject = esc(&commit.subject),
        oid = commit.oid.to_hex(),
        abbrev = esc(&commit.abbrev),
        author = esc(&commit.data.author),
        committer = esc(&commit.data.committer),
        message = esc(&commit.data.message),
    );
    page_shell(&commit.subject, &body)
}

fn render_tree(oid: &ObjectId, rows: &[grit_lib::instaweb::TreeRow]) -> String {
    let tree_html = render_tree_rows(rows);
    let body = format!(
        r#"<h1>Tree</h1>
<p class="mono">{oid}</p>
<table><thead><tr><th>Mode</th><th>Name</th><th>Object</th></tr></thead><tbody>{tree_html}</tbody></table>"#,
        oid = esc(&oid.to_hex()),
    );
    page_shell("Tree", &body)
}

fn render_tree_rows(rows: &[grit_lib::instaweb::TreeRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let link = match row.kind {
            TreeEntryKind::Tree => format!("/tree/{}", row.oid.to_hex()),
            TreeEntryKind::Blob => format!("/blob/{}", row.oid.to_hex()),
            TreeEntryKind::Other => format!("/blob/{}", row.oid.to_hex()),
        };
        out.push_str(&format!(
            "<tr><td class=\"mono\">{}</td><td><a href=\"{}\">{}</a></td><td class=\"mono\">{}</td></tr>",
            esc(&row.mode),
            link,
            esc(&row.name),
            esc(&row.oid.to_hex()[..7.min(row.oid.to_hex().len())])
        ));
    }
    out
}

fn render_blob(blob: &grit_lib::instaweb::BlobView) -> String {
    let content = if blob.is_text {
        format!(
            "<pre>{}</pre>{}",
            esc(&blob.text),
            if blob.truncated {
                "<p class=\"desc\">(truncated)</p>"
            } else {
                ""
            }
        )
    } else {
        format!("<p class=\"desc\">Binary blob ({} bytes)</p>", blob.size)
    };
    let body = format!(
        r#"<h1>Blob</h1>
<p class="mono">{}</p>
{content}"#,
        esc(&blob.oid.to_hex()),
    );
    page_shell("Blob", &body)
}
