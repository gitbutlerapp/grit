//! `gritx-push` — push to a repository's default remote, discovering the
//! transport and authentication from the remote's configured URL.
//!
//! Like `gritx-fetch`, it selects the remote (explicit arg, else the current
//! branch's upstream, else `origin`), classifies the URL, reports the auth it
//! will use, and runs the push in-process over grit-lib's transports. With no
//! refspec it pushes the current branch to the same-named branch on the remote;
//! a `[+]<src>[:<dst>]` argument (empty `<src>` deletes `<dst>`) overrides that.

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use clap::Parser;
use grit_examples::remote;
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::push_report::PushRefStatus;
use grit_lib::repo::Repository;
use grit_lib::transfer::PushOptions;
use grit_lib::transfer::PushRefSpec;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-push",
    version,
    about = "Push to the default remote, auto-discovering transport + auth"
)]
struct Cli {
    /// Remote to push to (default: the current branch's remote, else `origin`).
    remote: Option<String>,

    /// Refspec `[+]<src>[:<dst>]` (empty `<src>`, i.e. `:<dst>`, deletes).
    /// Default: the current branch to its same-named branch on the remote.
    refspec: Option<String>,

    /// Allow a non-fast-forward update (force push).
    #[arg(long, short)]
    force: bool,

    /// Server-side push option(s), exposed to the remote's hooks
    /// (`git push --push-option`). May be repeated.
    #[arg(long = "push-option", short = 'o', value_name = "VALUE")]
    push_option: Vec<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repository::discover(None)?;
    let git_dir = repo.git_dir.clone();
    let config = ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )?;

    let r = remote::resolve_remote(&config, &git_dir, cli.remote.as_deref(), true)?;

    let spec = match cli.refspec.as_deref() {
        Some(s) => parse_refspec(&git_dir, s, cli.force)?,
        None => current_branch_spec(&git_dir, cli.force)?,
    };

    eprintln!("Pushing to '{}' <{}>", r.name, r.url);
    eprintln!("  transport: {}", r.kind.label());
    eprintln!("  auth:      {}", remote::describe_auth(&config, &r));
    eprintln!(
        "  update:    {} -> {}{}",
        spec.src.map(short).unwrap_or_else(|| "(delete)".to_owned()),
        spec.dst,
        if spec.force { " (forced)" } else { "" }
    );

    let opts = PushOptions {
        push_options: cli.push_option.clone(),
        ..Default::default()
    };

    let outcome = remote::push(&repo, &r, std::slice::from_ref(&spec), &opts)?;

    let mut rejected = false;
    for res in &outcome.results {
        let (mark, note) = status_display(&res.status, res.message.as_deref());
        if is_reject(&res.status) {
            rejected = true;
        }
        let from = res.old_oid.map(short).unwrap_or_else(|| "(new)".to_owned());
        let to = if res.deletion {
            "(deleted)".to_owned()
        } else {
            res.new_oid.map(short).unwrap_or_else(|| "?".to_owned())
        };
        println!("  {mark} {}  {from}..{to}{note}", res.remote_ref);
    }
    if rejected {
        bail!("some refs were not pushed");
    }
    Ok(())
}

/// Build the default refspec: the current branch to its same-named branch.
fn current_branch_spec(git_dir: &std::path::Path, force: bool) -> Result<PushRefSpec> {
    let head = grit_lib::refs::read_symbolic_ref(git_dir, "HEAD")?
        .context("HEAD is detached; specify a refspec to push")?;
    let branch = head
        .strip_prefix("refs/heads/")
        .context("HEAD is not on a branch; specify a refspec to push")?;
    let oid = grit_lib::refs::resolve_ref(git_dir, &head)
        .with_context(|| format!("cannot resolve {head}"))?;
    Ok(PushRefSpec {
        src: Some(oid),
        dst: format!("refs/heads/{branch}"),
        force,
        delete: false,
        expected_old: None,
        expect_absent: false,
    })
}

/// Parse a `[+]<src>[:<dst>]` refspec (`:<dst>` / empty `<src>` deletes `<dst>`).
fn parse_refspec(git_dir: &std::path::Path, spec: &str, force_flag: bool) -> Result<PushRefSpec> {
    let (force, body) = match spec.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (force_flag, spec),
    };
    let (src, dst) = match body.split_once(':') {
        Some((s, d)) => (s, d),
        None => (body, body),
    };

    if src.is_empty() {
        // `:<dst>` — delete the destination ref.
        if dst.is_empty() {
            bail!("invalid refspec '{spec}'");
        }
        return Ok(PushRefSpec {
            src: None,
            dst: normalize_ref(dst),
            force,
            delete: true,
            expected_old: None,
            expect_absent: false,
        });
    }

    let src_ref = normalize_ref(src);
    let oid = grit_lib::refs::resolve_ref(git_dir, &src_ref)
        .or_else(|_| grit_lib::refs::resolve_ref(git_dir, src))
        .or_else(|_| src.parse::<ObjectId>().map_err(anyhow::Error::from))
        .with_context(|| format!("cannot resolve source '{src}'"))?;
    Ok(PushRefSpec {
        src: Some(oid),
        dst: normalize_ref(dst),
        force,
        delete: false,
        expected_old: None,
        expect_absent: false,
    })
}

/// Qualify a short branch name into a full `refs/heads/...` ref.
fn normalize_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

fn short(oid: ObjectId) -> String {
    oid.to_hex().chars().take(10).collect()
}

fn is_reject(status: &PushRefStatus) -> bool {
    !matches!(status, PushRefStatus::Ok | PushRefStatus::UpToDate)
}

/// A git-like status marker plus a parenthetical reason for rejections.
fn status_display(status: &PushRefStatus, message: Option<&str>) -> (&'static str, String) {
    let mark = match status {
        PushRefStatus::Ok => "*",
        PushRefStatus::UpToDate => "=",
        _ => "!",
    };
    let reason = match status {
        PushRefStatus::Ok | PushRefStatus::UpToDate => String::new(),
        PushRefStatus::RejectNonFastForward => "  [rejected] (non-fast-forward)".to_owned(),
        PushRefStatus::RejectAlreadyExists => "  [rejected] (already exists)".to_owned(),
        PushRefStatus::RejectFetchFirst => "  [rejected] (fetch first)".to_owned(),
        PushRefStatus::RejectNeedsForce => "  [rejected] (needs force)".to_owned(),
        PushRefStatus::RejectStale => "  [rejected] (stale info)".to_owned(),
        PushRefStatus::AtomicPushFailed => "  [rejected] (atomic push failed)".to_owned(),
        PushRefStatus::RemoteRejected => {
            format!("  [remote rejected] ({})", message.unwrap_or("declined"))
        }
    };
    (mark, reason)
}
