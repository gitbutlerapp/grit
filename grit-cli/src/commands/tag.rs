//! `grit tag` — list tags, or create / delete one.

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ref_name_messages::tag_short_name_error_message;
use anyhow::{bail, Context, Result};
use grit_lib::commit::signing::{should_sign_tag, GpgConfig, TrustLevel};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::tag::{
    create_tag, delete_tag, list_tags, CreateTagRequest, TagError, TagKind,
};
use grit_lib::refs::check_ref_format::validate_tag_short_name;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head_for_repository, HeadState};
use grit_lib::tree_sha256::{
    parse_tree_sha256_tag_headers, MissingSubmodule, MissingSubmoduleReason, SignatureVerdict,
    TagVerifyOptions, TreeSha256Verdict,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Result of `grit tag`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TagOutcome {
    /// All tags in the repository, sorted by name.
    List { tags: Vec<TagEntry> },
    /// A new tag was created.
    Create {
        name: String,
        /// Hex object id the tag ref points at.
        oid: String,
        kind: TagRefKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        peeled: Option<String>,
        /// Whether the tag object carries a cryptographic signature.
        signed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        replaced: Option<String>,
        dry_run: bool,
    },
    /// A tag was deleted.
    Delete { name: String, dry_run: bool },
    /// One or more annotated tags were verified.
    Verify {
        /// True when every listed tag passed verification.
        ok: bool,
        tags: Vec<TagVerifyEntry>,
    },
}

/// Per-tag result for `grit tag --verify`.
#[derive(Serialize, JsonSchema)]
pub struct TagVerifyEntry {
    pub name: String,
    /// Hex object id of the annotated tag object.
    pub oid: String,
    /// Hex object id from the tag's `object` header.
    pub target: String,
    pub ok: bool,
    pub signature: TagSignatureJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tree_sha256: Option<TagTreeSha256Json>,
    /// Submodule paths that could not be hashed for digest verification.
    pub not_verifiable_submodules: Vec<NotVerifiableSubmoduleJson>,
    /// Wall time spent verifying this tag.
    pub elapsed: TagVerifyTimingJson,
}

#[derive(Serialize, JsonSchema)]
pub struct TagSignatureJson {
    /// `good`, `bad`, `missing`, or `not_checked`.
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub trust: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct TagTreeSha256Json {
    pub claimed: String,
    pub computed: String,
    pub match_digest: bool,
    pub files: usize,
    pub submodules: usize,
    pub excluded: Vec<ExcludedSubmoduleJson>,
    pub bogus_excluded: Vec<String>,
    pub cycles: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ExcludedSubmoduleJson {
    pub path: String,
    pub oid: String,
}

#[derive(Serialize, JsonSchema)]
pub struct NotVerifiableSubmoduleJson {
    pub path: String,
    pub reason: String,
}

#[derive(Serialize, JsonSchema)]
pub struct TagVerifyTimingJson {
    /// Elapsed milliseconds (rounded).
    pub ms: u64,
}

/// Tag ref kind in JSON (`lightweight` or `annotated`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TagRefKind {
    Lightweight,
    Annotated,
}

impl TagRefKind {
    fn label(self) -> &'static str {
        match self {
            Self::Lightweight => "lightweight",
            Self::Annotated => "annotated",
        }
    }
}

impl From<TagKind> for TagRefKind {
    fn from(kind: TagKind) -> Self {
        match kind {
            TagKind::Lightweight => Self::Lightweight,
            TagKind::Annotated => Self::Annotated,
        }
    }
}

impl std::fmt::Display for TagRefKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One tag in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct TagEntry {
    pub name: String,
    /// Hex object id the tag ref points at.
    pub oid: String,
    pub kind: TagRefKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub peeled: Option<String>,
}

impl HumanRender for TagOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            TagOutcome::List { tags } => {
                if tags.is_empty() {
                    outln!(w, "No tags yet.");
                    return Ok(());
                }
                for tag in tags {
                    outln!(w, "  {}", tag.name);
                }
            }
            TagOutcome::Create { name, dry_run, .. } => {
                if *dry_run {
                    outln!(w, "Would create tag {name}");
                } else {
                    outln!(w, "Created tag {name}");
                }
            }
            TagOutcome::Delete { name, dry_run } => {
                if *dry_run {
                    outln!(w, "Would delete tag {name}");
                } else {
                    outln!(w, "Deleted tag {name}");
                }
            }
            TagOutcome::Verify { ok, tags } => {
                for entry in tags {
                    render_verify_human(w, entry)?;
                }
                if !*ok {
                    outln!(w, "Verification failed.");
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for TagOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            TagOutcome::List { tags } => markdown::print_tag_list(w, tags)?,
            TagOutcome::Create {
                name,
                oid,
                kind,
                peeled,
                signed,
                replaced,
                dry_run,
            } => {
                let short = oid.get(..7).unwrap_or(oid.as_str());
                let kind_note = match (*kind, *signed) {
                    (TagRefKind::Annotated, true) => " (annotated, signed)",
                    (TagRefKind::Annotated, false) => " (annotated)",
                    _ => "",
                };
                let peel = peeled
                    .as_ref()
                    .map(|p| format!(" → `{}`", p.get(..7).unwrap_or(p.as_str())))
                    .unwrap_or_default();
                let verb = if *dry_run { "Would create" } else { "Created" };
                outln!(w, "{verb} tag `{name}`{kind_note} at `{short}`{peel}.");
                if let Some(old) = replaced {
                    let old_short = old.get(..7).unwrap_or(old.as_str());
                    outln!(w, "Replaced previous target `{old_short}`.");
                }
            }
            TagOutcome::Delete { name, dry_run } => {
                let verb = if *dry_run { "Would delete" } else { "Deleted" };
                outln!(w, "{verb} tag `{name}`.");
            }
            TagOutcome::Verify { ok, tags } => {
                outln!(w, "## Tag verify\n");
                outln!(w, "- **ok:** {ok}\n");
                for entry in tags {
                    render_verify_markdown(w, entry)?;
                }
            }
        }
        Ok(())
    }
}

/// Parsed flags for a single `grit tag` invocation.
#[derive(Clone)]
pub struct TagRunOptions {
    /// Tag name, or none to list.
    pub name: Option<String>,
    /// Positional or `--object` revision to tag.
    pub target: Option<String>,
    /// Hidden `--object` alias.
    pub object: Option<String>,
    /// Delete instead of create.
    pub delete: bool,
    /// `-m` message.
    pub message: Option<String>,
    /// `-F` message file.
    pub message_file: Option<String>,
    /// `-f` replace existing tag.
    pub force: bool,
    /// Preview without writing refs.
    pub dry_run: bool,
    /// Sign override from `-s` / `--no-sign`.
    pub sign: Option<bool>,
    /// `--signing-key`.
    pub signing_key: Option<String>,
    /// Wall clock for tagger identity.
    pub now: OffsetDateTime,
    /// Verify annotated tags instead of creating or listing.
    pub verify: bool,
    /// Tag short names to verify (with `--verify`).
    pub verify_names: Vec<String>,
    /// Skip OpenPGP / SSH signature checking (digest still checked when present).
    pub no_signature: bool,
}

/// Run `grit tag` with the parsed CLI arguments.
pub fn run(opts: TagRunOptions) -> Result<TagOutcome> {
    let repo = context::discover()?;
    if opts.verify {
        return verify(&repo, &opts);
    }
    match &opts.name {
        None => list(&repo),
        Some(name) if opts.delete => {
            validate_delete_options(&opts)?;
            remove(&repo, name, opts.dry_run)
        }
        Some(name) => create(&repo, name, &opts),
    }
}

/// Exit code for `grit tag --verify`: 0 when every tag is OK, 1 otherwise.
#[must_use]
pub fn verify_exit_code(outcome: &TagOutcome) -> i32 {
    match outcome {
        TagOutcome::Verify { ok, .. } => i32::from(!*ok),
        _ => 0,
    }
}

fn validate_verify_options(opts: &TagRunOptions) -> Result<()> {
    if opts.verify_names.is_empty() {
        bail!("`--verify` requires at least one tag name");
    }
    if opts.delete {
        bail!("`--verify` cannot be used with `--delete`");
    }
    if opts.target.is_some() || opts.object.is_some() {
        bail!("unexpected revision argument with `--verify`");
    }
    if opts.message.is_some() || opts.message_file.is_some() {
        bail!("annotated tag options cannot be used with `--verify`");
    }
    if opts.force || opts.dry_run || opts.sign.is_some() || opts.signing_key.is_some() {
        bail!("create/delete options cannot be used with `--verify`");
    }
    Ok(())
}

fn validate_delete_options(opts: &TagRunOptions) -> Result<()> {
    if opts.target.is_some() {
        bail!("unexpected revision argument with `--delete`");
    }
    if opts.object.is_some() {
        bail!("unexpected `--object` with `--delete`");
    }
    if opts.message.is_some() || opts.message_file.is_some() {
        bail!("annotated tag options cannot be used with `--delete`");
    }
    Ok(())
}

fn verify(repo: &Repository, opts: &TagRunOptions) -> Result<TagOutcome> {
    validate_verify_options(opts)?;
    let config = repo.config().context("could not load config")?;
    let gpg = GpgConfig::from_config(config.as_ref())
        .context("could not load signing config")?
        .with_command_runner(repo.command_runner());
    let verify_opts = TagVerifyOptions {
        check_signature: !opts.no_signature,
        gpg,
    };

    let mut tags = Vec::with_capacity(opts.verify_names.len());
    let mut all_ok = true;
    for name in &opts.verify_names {
        let entry = verify_one_tag(repo, name, &verify_opts)?;
        all_ok &= entry.ok;
        tags.push(entry);
    }
    Ok(TagOutcome::Verify { ok: all_ok, tags })
}

fn verify_one_tag(
    repo: &Repository,
    name: &str,
    options: &TagVerifyOptions,
) -> Result<TagVerifyEntry> {
    if let Err(err) = validate_tag_short_name(name) {
        bail!(tag_short_name_error_message(name, &err));
    }
    let tag_ref = format!("refs/tags/{name}");
    let tag_oid = resolve_ref(repo.git_dir(), &tag_ref).map_err(|_| {
        map_tag_error(Error::Tag(TagError::NotFound {
            name: name.to_owned(),
        }))
    })?;
    let obj = repo
        .odb()
        .read(&tag_oid)
        .context("could not read tag object")?;
    if obj.kind != ObjectKind::Tag {
        bail!("tag '{name}' is lightweight (not an annotated tag object)");
    }
    let headers =
        parse_tree_sha256_tag_headers(&obj.data).context("could not parse tag headers")?;
    let target = headers.target.to_hex();

    let started = Instant::now();
    let verification = repo.verify_tag(tag_oid, options);
    let elapsed = timing_json(started.elapsed());

    match verification {
        Ok(report) => {
            let ok = report.is_ok();
            Ok(TagVerifyEntry {
                name: name.to_owned(),
                oid: tag_oid.to_hex(),
                target,
                ok,
                signature: signature_json(&report.signature),
                tree_sha256: report.tree_sha256.as_ref().map(tree_sha256_json),
                not_verifiable_submodules: report
                    .missing_submodules
                    .iter()
                    .map(not_verifiable_json)
                    .collect(),
                elapsed,
            })
        }
        Err(err) => Err(map_tag_error(err)),
    }
}

fn list(repo: &Repository) -> Result<TagOutcome> {
    let tags = list_tags(repo)
        .context("could not list tags")?
        .into_iter()
        .map(|tag| TagEntry {
            name: tag.name,
            oid: tag.oid.to_hex(),
            kind: tag.kind.into(),
            peeled: tag.peeled.map(|p| p.to_hex()),
        })
        .collect();
    Ok(TagOutcome::List { tags })
}

fn create(repo: &Repository, name: &str, opts: &TagRunOptions) -> Result<TagOutcome> {
    if let Err(err) = validate_tag_short_name(name) {
        bail!(tag_short_name_error_message(name, &err));
    }

    let rev = opts.target.clone().or_else(|| opts.object.clone());
    let target_oid = if let Some(rev) = rev {
        resolve_revision(repo, &rev).context("could not resolve tag target")?
    } else {
        match resolve_head_for_repository(repo)? {
            HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
            HeadState::Branch { .. } => bail!("no commits yet to tag"),
            HeadState::Invalid => bail!("HEAD is in an unknown state"),
        }
    };

    let message = read_message(opts.message.clone(), opts.message_file.clone())?;

    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let will_sign =
        should_sign_tag(&config, opts.sign).context("could not read tag signing config")?;
    let needs_annotated = message.is_some() || will_sign;
    let tagger = if needs_annotated {
        Some(
            context::identity(&env, &config, IdentRole::Committer, opts.now)
                .context("could not resolve tagger identity")?,
        )
    } else {
        None
    };

    let outcome = create_tag(
        repo,
        &CreateTagRequest {
            name: name.to_owned(),
            target: target_oid,
            message,
            tagger,
            sign: opts.sign,
            signing_key: opts.signing_key.clone(),
            force: opts.force,
            dry_run: opts.dry_run,
        },
    )
    .map_err(map_tag_error)?;

    Ok(TagOutcome::Create {
        name: outcome.name,
        oid: outcome.oid.to_hex(),
        kind: outcome.kind.into(),
        peeled: outcome.peeled.map(|p| p.to_hex()),
        signed: outcome.signed,
        replaced: outcome.replaced.map(|p| p.to_hex()),
        dry_run: outcome.dry_run,
    })
}

fn remove(repo: &Repository, name: &str, dry_run: bool) -> Result<TagOutcome> {
    delete_tag(repo, name, dry_run).map_err(map_tag_error)?;
    Ok(TagOutcome::Delete {
        name: name.to_owned(),
        dry_run,
    })
}

fn read_message(message: Option<String>, message_file: Option<String>) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(message), _) => Ok(Some(message)),
        (None, None) => Ok(None),
        (None, Some(path)) if path == "-" => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("could not read message from stdin")?;
            Ok(Some(buf))
        }
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map(Some)
            .with_context(|| format!("could not read message file '{path}'")),
    }
}

fn signature_json(verdict: &SignatureVerdict) -> TagSignatureJson {
    match verdict {
        SignatureVerdict::Good { signer, key, trust } => TagSignatureJson {
            verdict: "good".to_owned(),
            signer: signer.clone(),
            key: key.clone(),
            trust: Some(trust_level_label(*trust).to_owned()),
        },
        SignatureVerdict::Bad => TagSignatureJson {
            verdict: "bad".to_owned(),
            signer: None,
            key: None,
            trust: None,
        },
        SignatureVerdict::Missing => TagSignatureJson {
            verdict: "missing".to_owned(),
            signer: None,
            key: None,
            trust: None,
        },
        SignatureVerdict::NotChecked => TagSignatureJson {
            verdict: "not_checked".to_owned(),
            signer: None,
            key: None,
            trust: None,
        },
    }
}

fn tree_sha256_json(v: &TreeSha256Verdict) -> TagTreeSha256Json {
    TagTreeSha256Json {
        claimed: v.claimed.to_hex(),
        computed: v.computed.to_hex(),
        match_digest: v.claimed == v.computed && v.bogus_excluded.is_empty(),
        files: v.files,
        submodules: v.submodules,
        excluded: v
            .excluded
            .iter()
            .map(|(path, oid)| ExcludedSubmoduleJson {
                path: path.clone(),
                oid: oid.to_hex(),
            })
            .collect(),
        bogus_excluded: v.bogus_excluded.clone(),
        cycles: v.cycles.clone(),
    }
}

fn not_verifiable_json(m: &MissingSubmodule) -> NotVerifiableSubmoduleJson {
    NotVerifiableSubmoduleJson {
        path: m.path.clone(),
        reason: match m.reason {
            MissingSubmoduleReason::NotCheckedOut => "not_checked_out",
            MissingSubmoduleReason::MissingCommit => "missing_commit",
            _ => "unknown",
        }
        .to_owned(),
    }
}

fn timing_json(elapsed: Duration) -> TagVerifyTimingJson {
    TagVerifyTimingJson {
        ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
    }
}

fn trust_level_label(level: TrustLevel) -> &'static str {
    level.display_key()
}

fn render_verify_human(w: &mut dyn Write, entry: &TagVerifyEntry) -> Result<()> {
    let status = if entry.ok { "ok" } else { "FAILED" };
    outln!(
        w,
        "tag {name} ({oid}): {status}",
        name = entry.name,
        oid = entry.oid
    );
    outln!(w, "  target: {}", entry.target);
    outln!(
        w,
        "  signature: {} ({})",
        entry.signature.verdict,
        entry.signature.signer.as_deref().unwrap_or("—")
    );
    if let Some(digest) = &entry.tree_sha256 {
        outln!(
            w,
            "  tree-sha256: claimed {} computed {} ({})",
            digest.claimed,
            digest.computed,
            if digest.match_digest {
                "match"
            } else {
                "mismatch"
            }
        );
        outln!(
            w,
            "  hashed: {} files, {} submodules",
            digest.files,
            digest.submodules
        );
    }
    if !entry.not_verifiable_submodules.is_empty() {
        outln!(w, "  not verifiable submodules:");
        for miss in &entry.not_verifiable_submodules {
            outln!(w, "    {} ({})", miss.path, miss.reason);
        }
    }
    outln!(w, "  elapsed: {} ms", entry.elapsed.ms);
    Ok(())
}

fn render_verify_markdown(w: &mut dyn Write, entry: &TagVerifyEntry) -> Result<()> {
    outln!(w, "### `{}`\n", entry.name);
    outln!(w, "- **ok:** {}", entry.ok);
    outln!(w, "- **oid:** `{}`", entry.oid);
    outln!(w, "- **target:** `{}`", entry.target);
    outln!(w, "- **signature:** `{}`", entry.signature.verdict);
    if let Some(signer) = &entry.signature.signer {
        outln!(w, "- **signer:** `{signer}`");
    }
    if let Some(digest) = &entry.tree_sha256 {
        outln!(
            w,
            "- **tree-sha256:** claimed `{}`, computed `{}` ({})",
            digest.claimed,
            digest.computed,
            if digest.match_digest {
                "match"
            } else {
                "mismatch"
            }
        );
        outln!(
            w,
            "- **hashed:** {} files, {} submodules",
            digest.files,
            digest.submodules
        );
    }
    if !entry.not_verifiable_submodules.is_empty() {
        outln!(w, "- **not verifiable submodules:**");
        for miss in &entry.not_verifiable_submodules {
            outln!(w, "  - `{}` ({})", miss.path, miss.reason);
        }
    }
    outln!(w, "- **elapsed:** {} ms\n", entry.elapsed.ms);
    Ok(())
}

fn map_tag_error(err: Error) -> anyhow::Error {
    match err {
        Error::TagVerifyNotTag { .. } => {
            anyhow::anyhow!("tag is not an annotated tag object (use an annotated tag)")
        }
        Error::Tag(tag_err) => match tag_err {
            TagError::AlreadyExists { name } => {
                anyhow::anyhow!("tag '{name}' already exists (use --force to replace)")
            }
            TagError::InvalidName { detail } => anyhow::anyhow!("invalid tag name: {detail}"),
            TagError::NotFound { name } => anyhow::anyhow!("no tag named '{name}'"),
            TagError::EmptyMessage => anyhow::anyhow!("tag message cannot be empty"),
        },
        other => anyhow::Error::msg(other.to_string()),
    }
}
