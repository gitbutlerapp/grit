//! Accepting pushes (`receive-pack`).

use std::collections::HashSet;
use std::io::{BufReader, BufWriter, Read, Write};

use super::{
    advertised_refs, check_object_format, parse_oid, read_packet, ref_update_policy, write_flush,
    write_line, Packet, Peel, ProtocolVersion, Result, ServeError, ServeOptions,
};
use crate::config::ConfigSet;
use crate::fsck_msg_policy::{ObjectFsckPolicy, SkipListError};
use crate::fsck_standalone::FsckError;
use crate::hooks::{run_hook_in_git_dir, HookResult};
use crate::index_pack::{install_pack_path, IngestPackOptions};
use crate::objects::pack_stream_scan::{PackStreamScanError, PackStreamScanner};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp};
use crate::progress::{Cancel, CancelSignal, NeverCancel};
use crate::receive_pack::{max_input_size_from_config, should_use_unpack_objects};
use crate::receive_quarantine::ReceiveQuarantine;
use crate::repo::Repository;
use crate::shallow::{add_shallow_boundaries, load_shallow_boundaries, PushShallowInfo};
use crate::unpack_objects::{unpack_objects, UnpackOptions};

/// Bytes read from the client per call while spooling a pushed pack.
const SPOOL_CHUNK: usize = 256 * 1024;

/// Capabilities offered in the receive-pack advertisement, before the
/// per-repository ones.
const CAPABILITIES: &[&str] = &[
    "report-status",
    "report-status-v2",
    "delete-refs",
    "side-band-64k",
    "quiet",
    "atomic",
    "ofs-delta",
];

/// Which pushed objects and ref updates the server refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivePolicy {
    /// Refuse branch (`refs/heads/*`) updates that would discard commits
    /// (`receive.denyNonFastForwards`). Other refs are not checked.
    pub deny_non_fast_forwards: bool,
    /// Refuse ref deletions (`receive.denyDeletes`).
    pub deny_deletes: bool,
    /// In a repository with a working tree, refuse to update or delete the
    /// branch `HEAD` points at (`receive.denyCurrentBranch`).
    pub deny_current_branch: bool,
    /// Refuse to delete the branch `HEAD` points at, in a bare repository as
    /// well as one with a working tree (`receive.denyDeleteCurrent`).
    pub deny_delete_current: bool,
    /// Check every pushed commit, tree and tag with this policy before any of
    /// them enters the repository (`receive.fsckObjects`). One refused object
    /// refuses the whole push with [`UnpackError::Rejected`]. `None` skips the
    /// check.
    pub fsck_objects: Option<ObjectFsckPolicy>,
    /// Accept pushes from a shallow client that would make this repository
    /// shallow at one of the client's boundaries (`receive.shallowUpdate`).
    /// The new boundaries are added to the repository's `shallow` file.
    /// When false, such updates are refused with
    /// [`RefUpdateRefusal::ShallowUpdateNotAllowed`]; pushes whose history the
    /// repository already holds below the client's boundaries are accepted
    /// either way.
    pub shallow_update: bool,
}

impl Default for ReceivePolicy {
    fn default() -> Self {
        Self {
            deny_non_fast_forwards: false,
            deny_deletes: false,
            deny_current_branch: true,
            deny_delete_current: true,
            fsck_objects: None,
            shallow_update: false,
        }
    }
}

impl ReceivePolicy {
    /// Read the policy from the `receive.*` configuration keys.
    ///
    /// `receive.denyCurrentBranch` accepts Git's values: `refuse`, `true` or
    /// unset refuse the update; `ignore`, `warn`, `false` and `updateInstead`
    /// allow it (this server does not update the working tree).
    /// `receive.denyDeleteCurrent` is the same, except that `updateInstead`
    /// refuses: there is no working tree to update for a deleted branch.
    ///
    /// Pushed objects are checked when `receive.fsckObjects` is true, or when it
    /// is unset and `transfer.fsckObjects` is true. A value that is not a
    /// boolean turns the check on rather than silently leaving it off. The
    /// check uses `receive.fsck.<msg-id>` overrides and `receive.fsck.skipList`
    /// (see [`ObjectFsckPolicy::from_config`]).
    ///
    /// # Parameters
    ///
    /// - `config`: the repository's loaded configuration.
    ///
    /// # Returns
    ///
    /// The policy, with defaults for unset or unparseable keys.
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let flag = |key: &str| config.get_bool(key).and_then(|r| r.ok()).unwrap_or(false);
        // Whether a `receive.deny*` key is set to a value that lets the update through.
        let allows = |key: &str, update_instead_allows: bool| {
            config.get(key).is_some_and(|v| {
                let v = v.to_ascii_lowercase();
                matches!(v.as_str(), "ignore" | "warn" | "false" | "no" | "off" | "0")
                    || (update_instead_allows && v == "updateinstead")
            })
        };
        let explicit = |key: &str| config.get_bool(key).map(|r| r.unwrap_or(true));
        let fsck = explicit("receive.fsckObjects")
            .or_else(|| explicit("transfer.fsckObjects"))
            .unwrap_or(false);
        Self {
            deny_non_fast_forwards: flag("receive.denyNonFastForwards"),
            deny_deletes: flag("receive.denyDeletes"),
            deny_current_branch: !allows("receive.denyCurrentBranch", true),
            deny_delete_current: !allows("receive.denyDeleteCurrent", false),
            fsck_objects: fsck.then(|| ObjectFsckPolicy::from_config(config, "receive.fsck")),
            shallow_update: flag("receive.shallowUpdate"),
        }
    }
}

/// A pushed object refused by [`ReceivePolicy::fsck_objects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedObject {
    /// The refused object.
    pub oid: ObjectId,
    /// Its type.
    pub kind: ObjectKind,
    /// The error-level problem found in it.
    pub error: FsckError,
}

impl std::fmt::Display for RejectedObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} refused: {}",
            self.kind,
            self.oid.to_hex(),
            self.error.report_line()
        )
    }
}

/// One `<old> <new> <ref>` update requested by a pushing client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefCommand {
    /// Full name of the ref to change, such as `refs/heads/main`.
    pub refname: String,
    /// Value the client expects the ref to have now; `None` to create it.
    pub old: Option<ObjectId>,
    /// Value the client asks for; `None` to delete the ref.
    pub new: Option<ObjectId>,
}

impl RefCommand {
    fn refused(&self, reason: RefUpdateRefusal) -> RefUpdateResult {
        RefUpdateResult {
            refname: self.refname.clone(),
            old: self.old,
            new: self.new,
            error: Some(reason),
        }
    }

    fn accepted(&self) -> RefUpdateResult {
        RefUpdateResult {
            refname: self.refname.clone(),
            old: self.old,
            new: self.new,
            error: None,
        }
    }
}

/// Why the server refused one ref update.
///
/// The [`Display`](std::fmt::Display) text is the reason sent to the client on
/// the update's `ng <ref> <reason>` status line, which Git clients show next to
/// the rejected ref.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RefUpdateRefusal {
    /// The ref name is outside `refs/` or not a valid ref name.
    #[error("funny refname")]
    InvalidRefName,
    /// The ref is hidden by `receive.hideRefs` / `transfer.hideRefs`.
    #[error("deny updating a hidden ref")]
    HiddenRef,
    /// The ref no longer has the value the client expected.
    #[error("stale info")]
    StaleInfo,
    /// Deletions are refused (`receive.denyDeletes`).
    #[error("deletion prohibited")]
    DeletionProhibited,
    /// The update would delete the branch `HEAD` points at
    /// (`receive.denyDeleteCurrent`).
    #[error("deletion of the current branch prohibited")]
    CurrentBranchDeletion,
    /// The update would move the checked-out branch
    /// (`receive.denyCurrentBranch`).
    #[error("branch is currently checked out")]
    CurrentBranch,
    /// The new value is not in the repository or the pushed pack.
    #[error("missing necessary objects")]
    MissingObjects,
    /// The branch update would discard commits (`receive.denyNonFastForwards`).
    #[error("non-fast-forward")]
    NonFastForward,
    /// A branch update under `receive.denyNonFastForwards` names a non-commit
    /// as its old or new value.
    #[error("bad ref")]
    BadRef,
    /// Under `receive.denyNonFastForwards`, the ancestry walk between the old
    /// and new branch tips failed, so the update could not be shown to be a
    /// fast-forward.
    #[error("unable to verify fast-forward")]
    FastForwardCheckFailed,
    /// The pushed history ends at a boundary of the shallow client that this
    /// repository does not hold the ancestry of, and
    /// [`ReceivePolicy::shallow_update`] is off.
    #[error("shallow update not allowed")]
    ShallowUpdateNotAllowed,
    /// The pushed pack could not be stored; see
    /// [`ReceiveOutcome::unpack_error`].
    #[error("unpacker error")]
    UnpackerError,
    /// Another update in an `atomic` push was refused, so this one was not
    /// applied either.
    #[error("atomic transaction failed")]
    AtomicTransactionFailed,
    /// The `pre-receive` hook script exited non-zero.
    #[error("pre-receive hook declined")]
    PreReceiveHookDeclined,
    /// The `update` hook script exited non-zero for this ref.
    #[error("hook declined")]
    UpdateHookDeclined,
    /// A [`ReceiveHook`] refused the update with this reason.
    #[error("{0}")]
    Policy(String),
    /// The ref is a served repository's target branch, which changes only
    /// through the server's own operations (snapshot checkouts' integrate and
    /// slices), never by a push.
    #[error("{target} is the target branch; it changes only through integrate")]
    TargetBranch {
        /// The target branch's name, without `refs/heads/`.
        target: String,
    },
    /// Writing the ref failed after every check passed.
    #[error("failed to update ref: {0}")]
    RefStore(String),
}

/// The result of one requested ref update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdateResult {
    /// Full name of the ref the client asked to change.
    pub refname: String,
    /// Value the client expected the ref to have; `None` for a create.
    pub old: Option<ObjectId>,
    /// Value the client asked for; `None` for a delete.
    pub new: Option<ObjectId>,
    /// Why the update was refused, or `None` when it was applied.
    pub error: Option<RefUpdateRefusal>,
}

/// Why a pushed pack could not be stored.
///
/// The [`Display`](std::fmt::Display) text is sent to the client on the
/// `unpack <reason>` status line. Every requested update is then refused with
/// [`RefUpdateRefusal::UnpackerError`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UnpackError {
    /// The client stopped sending before the pack was complete.
    #[error("unexpected end of pack stream")]
    Truncated,
    /// The pack is larger than `receive.maxInputSize`.
    #[error("pack exceeds maximum input size of {limit} bytes")]
    TooLarge {
        /// The configured limit in bytes.
        limit: u64,
    },
    /// The stream is not a well-formed pack.
    #[error(transparent)]
    Malformed(#[from] PackStreamScanError),
    /// The pack arrived but its objects could not be stored (bad checksum,
    /// corrupt object, failed connectivity check, ...).
    #[error(transparent)]
    Ingest(Box<crate::error::Error>),
    /// A pushed object failed [`ReceivePolicy::fsck_objects`].
    #[error("{0}")]
    Rejected(RejectedObject),
    /// The `receive.fsck.skipList` file could not be loaded, so no object
    /// could be checked.
    #[error(transparent)]
    FsckSkipList(#[from] SkipListError),
    /// Reading the client stream or writing the spooled pack failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The session was stopped by a cancel signal while reading the pack.
    #[error("cancelled")]
    Cancelled,
}

impl From<crate::error::Error> for UnpackError {
    fn from(err: crate::error::Error) -> Self {
        Self::Ingest(Box::new(err))
    }
}

/// In-process checks an embedding server runs on every push.
///
/// These run in addition to [`ReceivePolicy`] and the repository's hook scripts,
/// so a server can refuse updates from Rust code without installing a script.
/// Both methods default to accepting; implement the ones you need. A closure
/// `Fn(&RefCommand, &Odb) -> Result<(), RefUpdateRefusal>` implements the
/// per-ref [`Self::update`] check, and `()` accepts everything.
///
/// Refusals are usually [`RefUpdateRefusal::Policy`] with a reason the client
/// will see.
///
/// Push options from the client are passed in [`ReceiveHookContext`] to
/// [`Self::pre_receive_ctx`] and [`Self::update_ctx`]; the default methods ignore
/// them and call [`Self::pre_receive`] and [`Self::update`].
pub trait ReceiveHook {
    /// Inspect the whole push before any ref changes, like a `pre-receive` hook.
    ///
    /// Runs before the `pre-receive` script, while the pushed objects are still
    /// in quarantine.
    ///
    /// # Parameters
    ///
    /// - `commands`: the updates that passed the built-in checks, in request
    ///   order.
    /// - `odb`: an object database that sees the repository's objects and the
    ///   quarantined pushed ones.
    ///
    /// # Errors
    ///
    /// Returning a refusal refuses every update in `commands` with it.
    fn pre_receive(
        &self,
        commands: &[RefCommand],
        odb: &Odb,
    ) -> std::result::Result<(), RefUpdateRefusal> {
        let _ = (commands, odb);
        Ok(())
    }

    /// Like [`Self::pre_receive`], with push options from the client request.
    fn pre_receive_ctx(
        &self,
        commands: &[RefCommand],
        odb: &Odb,
        ctx: &ReceiveHookContext<'_>,
    ) -> std::result::Result<(), RefUpdateRefusal> {
        let _ = ctx;
        self.pre_receive(commands, odb)
    }

    /// Inspect one update, like an `update` hook.
    ///
    /// Runs before the `update` script for the same ref, after the pushed
    /// objects were moved into the repository.
    ///
    /// # Parameters
    ///
    /// - `command`: the update to check.
    /// - `odb`: the repository's object database, including the pushed objects.
    ///
    /// # Errors
    ///
    /// Returning a refusal refuses this update only (or the whole push when the
    /// client asked for an `atomic` push).
    fn update(&self, command: &RefCommand, odb: &Odb) -> std::result::Result<(), RefUpdateRefusal> {
        let _ = (command, odb);
        Ok(())
    }

    /// Like [`Self::update`], with push options from the client request.
    fn update_ctx(
        &self,
        command: &RefCommand,
        odb: &Odb,
        ctx: &ReceiveHookContext<'_>,
    ) -> std::result::Result<(), RefUpdateRefusal> {
        let _ = ctx;
        self.update(command, odb)
    }

    /// What to tell the pushing client once every update has been decided,
    /// as a hook script's output would: Git clients show each line as
    /// `remote: <line>`.
    ///
    /// The text goes out on the progress side band just before the status
    /// report, so only to clients that asked for a side band. The default
    /// sends nothing.
    ///
    /// # Parameters
    ///
    /// - `updates`: every requested update with its result, in request order.
    ///
    /// # Returns
    ///
    /// The text, or `None` for nothing.
    fn message(&self, updates: &[RefUpdateResult]) -> Option<String> {
        let _ = updates;
        None
    }
}

/// Push options and other session context for [`ReceiveHook`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ReceiveHookContext<'a> {
    /// Values the client sent after the command list (`push-options` capability).
    pub push_options: &'a [String],
}

impl ReceiveHook for () {}

impl<F> ReceiveHook for F
where
    F: Fn(&RefCommand, &Odb) -> std::result::Result<(), RefUpdateRefusal>,
{
    fn update(&self, command: &RefCommand, odb: &Odb) -> std::result::Result<(), RefUpdateRefusal> {
        self(command, odb)
    }
}

/// One server-side hook invocation during receive-pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunRecord {
    /// Hook name (`pre-receive`, `update`, `post-receive`, or `post-update`).
    pub name: String,
    /// Whether an executable hook ran (missing hooks are skipped).
    pub ran: bool,
    /// Exit code when [`Self::ran`] is true.
    pub exit_code: Option<i32>,
}

/// What a push session did.
#[derive(Debug, Default)]
pub struct ReceiveOutcome {
    /// Why the pushed pack could not be stored, when it could not.
    pub unpack_error: Option<UnpackError>,
    /// One entry per ref update the client requested, in request order.
    pub updates: Vec<RefUpdateResult>,
    /// Hook runs performed for this push, in execution order.
    pub hooks: Vec<HookRunRecord>,
    /// Push options delivered to hooks via `GIT_PUSH_OPTION_*`.
    pub push_options: Vec<String>,
}

/// Serve one push session: advertise refs, read the ref updates and pack, store
/// the objects, apply the updates, and report the result to the client.
///
/// The same as [`receive_pack_with_hook`] with no in-process [`ReceiveHook`].
///
/// # Errors
///
/// See [`receive_pack_with_hook`].
pub fn receive_pack(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
    policy: &ReceivePolicy,
) -> Result<ReceiveOutcome> {
    receive_pack_with_hook(repo, input, output, opts, policy, &())
}

/// Serve one push session, running `hook` alongside the hook scripts.
///
/// The pushed pack is streamed into a temp file inside a quarantine object
/// directory, so memory use does not grow with the pack size. Its objects are
/// then stored via [`unpack_objects`] for packs with fewer objects than
/// `receive.unpackLimit` and installed as a pack with an index otherwise,
/// matching Git receive-pack. On that unpack path, blobs larger than 1 MiB are
/// inflated and written as loose objects in fixed-size chunks, so peak memory
/// stays bounded rather than holding the full inflated payload. Updates are checked against `policy` and
/// against the value the client expected each ref to have; with the `atomic`
/// capability either every update is applied or none is. Server-side hooks run
/// through the repository [`CommandRunner`](crate::command_runner::CommandRunner)
/// (`pre-receive`, `update`, `post-receive`, `post-update`) with push options and
/// quarantined object ingestion matching Git receive-pack. `hook` runs just
/// before the matching script: [`ReceiveHook::pre_receive`] before
/// `pre-receive` and [`ReceiveHook::update`] before each `update`; its
/// [`ReceiveHook::message`] reaches a client that asked for a side band just
/// before the status report.
///
/// The pack is read only up to its trailing checksum, never to end of input:
/// a push client keeps its side open until it has read the status report.
///
/// A shallow client announces its boundaries with `shallow <oid>` lines before
/// the commands. Their parents are not required when the pushed objects are
/// checked. An update whose history ends at such a boundary that this
/// repository's refs do not already reach would make the repository shallow:
/// it is refused with [`RefUpdateRefusal::ShallowUpdateNotAllowed`] unless
/// [`ReceivePolicy::shallow_update`] is on, in which case the boundaries are
/// added to the `shallow` file before the ref is written and hook scripts see
/// the client's boundaries through `GIT_SHALLOW_FILE`.
///
/// # Parameters
///
/// - `repo`: the repository receiving the push.
/// - `input`: the client's command list and pack.
/// - `output`: where the advertisement and status report are written.
/// - `opts`: framing, protocol version, agent string, and hidden refs. Protocol
///   v2 is not defined for pushes, so it is served as v0.
/// - `policy`: which updates to refuse.
/// - `hook`: in-process checks; pass `&()` for none.
///
/// # Returns
///
/// What was stored and which updates were applied. A pack that could not be
/// stored ([`ReceiveOutcome::unpack_error`]) and refused updates
/// ([`RefUpdateResult::error`]) are reported to the client and in the outcome;
/// they are not errors.
///
/// # Errors
///
/// Returns [`ServeError::Protocol`] for a malformed command list, and I/O or
/// repository errors from writing the response, creating the quarantine, or
/// moving the received objects into the repository.
pub fn receive_pack_with_hook(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
    policy: &ReceivePolicy,
    hook: &dyn ReceiveHook,
) -> Result<ReceiveOutcome> {
    receive_pack_with_hook_cancellable(repo, input, output, opts, policy, hook, &NeverCancel)
}

/// Serve one push session with a cancel signal, running `hook` alongside hook scripts.
///
/// Same as [`receive_pack_with_hook`], but stops without moving refs when `cancel`
/// fires while reading the pack or before updates are applied.
///
/// # Errors
///
/// Returns [`ServeError::Cancelled`] when `cancel` is already set or fires during
/// the session before ref updates are committed.
pub fn receive_pack_with_hook_cancellable(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
    policy: &ReceivePolicy,
    hook: &dyn ReceiveHook,
    cancel: &dyn Cancel,
) -> Result<ReceiveOutcome> {
    let stop = CancelSignal::new(cancel);
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    if opts.advertise_refs || !opts.stateless_rpc {
        if opts.protocol == ProtocolVersion::V1 {
            write_line(output, "version 1")?;
        }
        write_advertisement(repo, output, opts)?;
        output.flush()?;
        if opts.advertise_refs {
            return Ok(ReceiveOutcome::default());
        }
    }

    let Some(request) = read_commands(repo, input)? else {
        return Ok(ReceiveOutcome::default());
    };

    let cfg = repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(ConfigSet::new()));
    let max_input = max_input_size_from_config(cfg.as_ref());
    let sideband = request.caps.contains("side-band-64k");

    let needs_pack = request.commands.iter().any(|c| c.new.is_some());
    let mut quarantine = if needs_pack {
        Some(ReceiveQuarantine::create(repo.odb(), repo.git_dir())?)
    } else {
        None
    };

    let receiver_grafts = if request.shallow.is_empty() {
        HashSet::new()
    } else {
        load_shallow_boundaries(repo.git_dir())
    };
    let mut shallow_info =
        PushShallowInfo::classify(&request.shallow, repo.odb(), &receiver_grafts)?;
    let client_boundaries: HashSet<ObjectId> = request.shallow.iter().copied().collect();

    let unpack_error = match quarantine.as_ref() {
        Some(q) => {
            let odb = q.odb();
            spool_push_pack(input, &odb, max_input, &stop)
                .and_then(|pack| ingest_spooled_pack(&odb, pack, cfg.as_ref(), &client_boundaries))
                .and_then(|indexed| match &policy.fsck_objects {
                    Some(fsck) => check_received_objects(repo, &odb, indexed, cfg.as_ref(), fsck),
                    None => Ok(()),
                })
                .err()
        }
        None => None,
    };
    if matches!(unpack_error, Some(UnpackError::Cancelled)) {
        return Err(ServeError::Cancelled);
    }

    let shallow = match quarantine.as_ref() {
        Some(q) if unpack_error.is_none() => {
            shallow_info.retain_received(&q.odb())?;
            ShallowPlan::new(
                repo,
                &q.odb(),
                &request,
                policy,
                &shallow_info,
                &receiver_grafts,
            )?
        }
        _ => ShallowPlan::none(request.commands.len()),
    };

    let mut hook_records = Vec::new();
    let updates = if unpack_error.is_some() {
        request
            .commands
            .iter()
            .map(|c| c.refused(RefUpdateRefusal::UnpackerError))
            .collect()
    } else {
        stop.poll().map_err(|_| ServeError::Cancelled)?;
        apply_commands_with_hooks(
            repo,
            &request,
            opts,
            policy,
            hook,
            quarantine.as_mut(),
            &shallow,
            sideband.then_some(&mut *output),
            &mut hook_records,
        )?
    };

    if sideband {
        if let Some(text) = hook.message(&updates) {
            write_progress_text(output, &text)?;
        }
    }
    if request.caps.contains("report-status") || request.caps.contains("report-status-v2") {
        let report = build_report(unpack_error.as_ref(), &updates)?;
        if sideband {
            crate::pkt_line::write_sideband_channel1_64k(&mut &mut *output, &report)?;
            write_flush(output)?;
        } else {
            output.write_all(&report)?;
        }
    }
    output.flush()?;

    Ok(ReceiveOutcome {
        unpack_error,
        updates,
        hooks: hook_records,
        push_options: request.push_options.clone(),
    })
}

/// Send `text` on the progress side band (channel 2), one line per packet,
/// as Git relays a hook script's output.
fn write_progress_text(output: &mut dyn Write, text: &str) -> Result<()> {
    for line in text.lines() {
        let line = format!("{line}\n");
        for chunk in line.as_bytes().chunks(65515) {
            crate::pkt_line::write_sideband_packet(&mut *output, 2, chunk)
                .map_err(ServeError::Io)?;
        }
    }
    Ok(())
}

/// Write the refs the client may update, with capabilities on the first line.
fn write_advertisement(
    repo: &Repository,
    output: &mut dyn Write,
    opts: &ServeOptions,
) -> Result<()> {
    let mut caps: Vec<String> = CAPABILITIES.iter().map(|c| (*c).to_owned()).collect();
    let config = repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(ConfigSet::new()));
    if config
        .get_bool("receive.advertisepushoptions")
        .and_then(|r| r.ok())
        .unwrap_or(false)
    {
        caps.push("push-options".to_owned());
    }
    caps.push(format!("object-format={}", repo.odb().hash_algo().name()));
    if let Some(path) = &opts.push_bundle {
        caps.push(format!("{}={path}", super::PUSH_BUNDLE_CAPABILITY));
    }
    if !opts.agent.is_empty() {
        caps.push(format!("agent={}", opts.agent));
    }
    let caps = caps.join(" ");

    let refs = advertised_refs(repo, &opts.hidden_refs, &[], Peel::No)?;
    if refs.is_empty() {
        let zero = ObjectId::null(repo.odb().hash_algo());
        write_line(
            output,
            &format!("{} capabilities^{{}}\0{caps}", zero.to_hex()),
        )?;
    }
    for (i, r) in refs.iter().enumerate() {
        if i == 0 {
            write_line(output, &format!("{} {}\0{caps}", r.oid.to_hex(), r.name))?;
        } else {
            write_line(output, &format!("{} {}", r.oid.to_hex(), r.name))?;
        }
    }
    write_flush(output)
}

/// The command list of a push.
struct PushRequest {
    commands: Vec<RefCommand>,
    caps: HashSet<String>,
    push_options: Vec<String>,
    /// The client's shallow boundaries, from `shallow <oid>` lines.
    shallow: Vec<ObjectId>,
}

/// Read the command list up to its flush. Returns `None` when the client sent
/// no commands (nothing to push).
///
/// A shallow client sends a `shallow <oid>` line for each of its boundaries
/// before the commands; they are collected wherever they appear in the list.
fn read_commands(repo: &Repository, input: &mut dyn Read) -> Result<Option<PushRequest>> {
    let mut commands = Vec::new();
    let mut caps = HashSet::new();
    let mut shallow = Vec::new();
    loop {
        match read_packet(input)? {
            None | Some(Packet::Flush) => break,
            Some(Packet::Data(line)) => {
                if let Some(hex) = line.strip_prefix("shallow ") {
                    shallow.push(parse_oid(repo, hex)?);
                    continue;
                }
                let (line, line_caps) = line.split_once('\0').unwrap_or((line.as_str(), ""));
                if commands.is_empty() {
                    caps.extend(
                        line_caps
                            .split(' ')
                            .filter(|c| !c.is_empty())
                            .map(str::to_owned),
                    );
                }
                let mut parts = line.splitn(3, ' ');
                let (Some(old), Some(new), Some(refname)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Err(ServeError::Protocol(format!("malformed command '{line}'")));
                };
                let old = parse_oid(repo, old)?;
                let new = parse_oid(repo, new)?;
                commands.push(RefCommand {
                    old: (!old.is_zero()).then_some(old),
                    new: (!new.is_zero()).then_some(new),
                    refname: refname.to_owned(),
                });
            }
            Some(other) => {
                return Err(ServeError::Protocol(format!(
                    "unexpected packet in command list: {other:?}"
                )))
            }
        }
    }
    if commands.is_empty() {
        return Ok(None);
    }
    let mut push_options = Vec::new();
    if caps.contains("push-options") {
        loop {
            match read_packet(input)? {
                None | Some(Packet::Flush) => break,
                Some(Packet::Data(line)) => {
                    push_options.push(line);
                }
                Some(other) => {
                    return Err(ServeError::Protocol(format!(
                        "unexpected packet in push options: {other:?}"
                    )))
                }
            }
        }
    }
    let format = caps.iter().find_map(|c| c.strip_prefix("object-format="));
    check_object_format(repo, format)?;
    Ok(Some(PushRequest {
        commands,
        caps,
        push_options,
        shallow,
    }))
}

/// The shallow-file changes a push from a shallow client needs.
struct ShallowPlan {
    /// For each command, in request order, the boundaries accepting it would
    /// add to the repository's `shallow` file.
    roots: Vec<Vec<ObjectId>>,
    /// Under [`ReceivePolicy::shallow_update`], a shallow file listing the
    /// repository's boundaries and the client's, which hooks see as
    /// `GIT_SHALLOW_FILE` so they can walk the pushed history.
    hook_file: Option<tempfile::NamedTempFile>,
}

impl ShallowPlan {
    /// No shallow-file changes for `commands` commands.
    fn none(commands: usize) -> Self {
        Self {
            roots: vec![Vec::new(); commands],
            hook_file: None,
        }
    }

    /// Work out the new boundaries of every command once the pushed objects
    /// are in `odb`.
    fn new(
        repo: &Repository,
        odb: &Odb,
        request: &PushRequest,
        policy: &ReceivePolicy,
        info: &PushShallowInfo,
        receiver_grafts: &HashSet<ObjectId>,
    ) -> Result<Self> {
        if info.is_empty() {
            return Ok(Self::none(request.commands.len()));
        }
        let mut receiver_tips: HashSet<ObjectId> = advertised_refs(repo, &[], &[], Peel::Yes)?
            .into_iter()
            .map(|r| r.peeled.unwrap_or(r.oid))
            .collect();
        receiver_tips.extend(crate::refs::resolve_ref(repo.git_dir(), "HEAD").ok());
        let graph = repo.odb().commit_graph_chain();
        let roots = request
            .commands
            .iter()
            .map(|c| match c.new {
                Some(new) => {
                    info.new_roots(odb, graph.as_deref(), new, receiver_grafts, &receiver_tips)
                }
                None => Ok(Vec::new()),
            })
            .collect::<crate::error::Result<_>>()?;
        let hook_file = if policy.shallow_update {
            Some(temporary_shallow_file(
                repo,
                receiver_grafts,
                &request.shallow,
            )?)
        } else {
            None
        };
        Ok(Self { roots, hook_file })
    }

    /// `GIT_SHALLOW_FILE` for hook scripts, when set.
    fn hook_env(&self) -> Result<Vec<(String, String)>> {
        let Some(file) = &self.hook_file else {
            return Ok(Vec::new());
        };
        let path = std::path::absolute(file.path())?;
        Ok(vec![(
            "GIT_SHALLOW_FILE".to_owned(),
            path.to_string_lossy().into_owned(),
        )])
    }
}

/// Write the union of `receiver_grafts` and `client` to a temporary shallow
/// file next to the repository's own.
fn temporary_shallow_file(
    repo: &Repository,
    receiver_grafts: &HashSet<ObjectId>,
    client: &[ObjectId],
) -> Result<tempfile::NamedTempFile> {
    let mut hexes: Vec<String> = receiver_grafts
        .iter()
        .chain(client)
        .map(ObjectId::to_hex)
        .collect();
    hexes.sort_unstable();
    hexes.dedup();
    let dir = crate::shallow::shallow_file_path(repo.git_dir())
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| repo.git_dir().to_path_buf());
    let mut file = tempfile::Builder::new()
        .prefix("shallow_")
        .tempfile_in(dir)?;
    for hex in hexes {
        writeln!(file, "{hex}")?;
    }
    file.flush()?;
    Ok(file)
}

/// Validate, run hooks, migrate quarantine objects, and apply ref updates.
#[allow(clippy::too_many_arguments)]
fn apply_commands_with_hooks(
    repo: &Repository,
    request: &PushRequest,
    opts: &ServeOptions,
    policy: &ReceivePolicy,
    hook: &dyn ReceiveHook,
    quarantine: Option<&mut ReceiveQuarantine>,
    shallow: &ShallowPlan,
    mut sideband_out: Option<&mut dyn Write>,
    hook_records: &mut Vec<HookRunRecord>,
) -> Result<Vec<RefUpdateResult>> {
    let lookup_odb = quarantine.as_ref().map(|q| q.odb());
    let head_branch = ref_update_policy::head_branch(repo);
    let checked_out = ref_update_policy::checked_out_branch(repo);
    let branches = ref_update_policy::HeadBranches {
        head: head_branch.as_deref(),
        checked_out: checked_out.as_deref(),
    };
    let mut results: Vec<RefUpdateResult> = request
        .commands
        .iter()
        .zip(&shallow.roots)
        .map(|(c, roots)| {
            if !roots.is_empty() && !policy.shallow_update {
                return Ok(c.refused(RefUpdateRefusal::ShallowUpdateNotAllowed));
            }
            Ok(
                match ref_update_policy::validate_ref_update(
                    repo,
                    lookup_odb.as_ref(),
                    c,
                    policy,
                    &opts.hidden_refs,
                    branches,
                )? {
                    Some(reason) => c.refused(reason),
                    None => c.accepted(),
                },
            )
        })
        .collect::<Result<_>>()?;

    let atomic = request.caps.contains("atomic");
    if atomic && results.iter().any(|r| r.error.is_some()) {
        refuse_remaining(&mut results, &RefUpdateRefusal::AtomicTransactionFailed);
        return Ok(results);
    }

    let accepted: Vec<RefCommand> = request
        .commands
        .iter()
        .zip(&results)
        .filter(|(_, r)| r.error.is_none())
        .map(|(c, _)| c.clone())
        .collect();
    let hook_ctx = ReceiveHookContext {
        push_options: &request.push_options,
    };
    if !accepted.is_empty() {
        if let Err(refusal) = hook.pre_receive_ctx(
            &accepted,
            lookup_odb.as_ref().unwrap_or(repo.odb()),
            &hook_ctx,
        ) {
            refuse_remaining(&mut results, &refusal);
            return Ok(results);
        }
    }

    let scripts = ReceiveHookScripts::new(repo, &request.push_options, shallow.hook_env()?);
    let all_cmds: Vec<&RefCommand> = request.commands.iter().collect();
    let pre_result = scripts.pre_receive(&all_cmds, quarantine.as_deref());
    relay_hook_sideband(&mut sideband_out, &pre_result)?;
    hook_records.push(hook_record("pre-receive", &pre_result));
    if !pre_result.is_ok() {
        refuse_remaining(&mut results, &RefUpdateRefusal::PreReceiveHookDeclined);
        return Ok(results);
    }

    if results.iter().all(|r| r.error.is_some()) {
        return Ok(results);
    }

    let mut migrated_pack_keeps = Vec::new();
    if let Some(q) = quarantine {
        migrated_pack_keeps = q.migrate().map_err(ServeError::Repository)?;
        repo.odb().invalidate_packs();
    }

    for (cmd, result) in request.commands.iter().zip(results.iter_mut()) {
        if result.error.is_some() {
            continue;
        }
        if let Err(refusal) = hook.update_ctx(cmd, repo.odb(), &hook_ctx) {
            result.error = Some(refusal);
            continue;
        }
        let update_result = scripts.update(cmd);
        relay_hook_sideband(&mut sideband_out, &update_result)?;
        hook_records.push(hook_record("update", &update_result));
        if !update_result.is_ok() {
            result.error = Some(RefUpdateRefusal::UpdateHookDeclined);
        }
    }

    if atomic && results.iter().any(|r| r.error.is_some()) {
        refuse_remaining(&mut results, &RefUpdateRefusal::AtomicTransactionFailed);
        return Ok(results);
    }

    let new_roots: HashSet<ObjectId> = shallow
        .roots
        .iter()
        .zip(&results)
        .filter(|(_, r)| r.error.is_none())
        .flat_map(|(roots, _)| roots.iter().copied())
        .collect();
    if let Err(e) = add_shallow_boundaries(repo.git_dir(), &new_roots) {
        let refusal = RefUpdateRefusal::RefStore(e.to_string());
        for (roots, result) in shallow.roots.iter().zip(results.iter_mut()) {
            if !roots.is_empty() && result.error.is_none() {
                result.error = Some(refusal.clone());
            }
        }
        if atomic {
            refuse_remaining(&mut results, &RefUpdateRefusal::AtomicTransactionFailed);
            return Ok(results);
        }
    }

    let pending: Vec<(usize, crate::refs::store::RefUpdate)> = request
        .commands
        .iter()
        .zip(results.iter())
        .enumerate()
        .filter(|(_, (_, r))| r.error.is_none())
        .map(|(idx, (cmd, _))| (idx, receive_command_store_update(cmd)))
        .collect();

    if atomic {
        let batch: Vec<_> = pending.iter().map(|(_, u)| u.clone()).collect();
        if let Err(e) = crate::refs::commit_ref_updates_on_store(repo.refs(), &batch, &[]) {
            let refusal = RefUpdateRefusal::RefStore(e.to_string());
            for (idx, _) in pending {
                if results[idx].error.is_none() {
                    results[idx].error = Some(refusal.clone());
                }
            }
        }
    } else {
        for (idx, update) in pending {
            if let Err(e) = crate::refs::commit_ref_update_on_store(repo.refs(), update) {
                results[idx].error = Some(RefUpdateRefusal::RefStore(e.to_string()));
            }
        }
    }

    let successful: Vec<&RefCommand> = request
        .commands
        .iter()
        .zip(results.iter())
        .filter(|(_, r)| r.error.is_none())
        .map(|(c, _)| c)
        .collect();
    if !successful.is_empty() {
        let post_result = scripts.post_receive(&successful);
        relay_hook_sideband(&mut sideband_out, &post_result)?;
        hook_records.push(hook_record("post-receive", &post_result));

        let pu_result = scripts.post_update(&successful);
        relay_hook_sideband(&mut sideband_out, &pu_result)?;
        hook_records.push(hook_record("post-update", &pu_result));
    }

    drop(migrated_pack_keeps);

    Ok(results)
}

/// Refuse every update that has not been refused yet.
fn refuse_remaining(results: &mut [RefUpdateResult], reason: &RefUpdateRefusal) {
    for r in results.iter_mut().filter(|r| r.error.is_none()) {
        r.error = Some(reason.clone());
    }
}

fn hook_record(name: &str, result: &HookResult) -> HookRunRecord {
    HookRunRecord {
        name: name.to_owned(),
        ran: result.was_executed(),
        exit_code: result.status(),
    }
}

/// The receiving repository's hook scripts for one push, run with Git's arguments, stdin
/// and environment.
///
/// Every script runs from the repository directory with `GIT_DIR=.` (see
/// [`run_hook_in_git_dir`]) and its output captured. `pre-receive` and `post-receive` get the
/// push options as `GIT_PUSH_OPTION_COUNT` / `GIT_PUSH_OPTION_<n>`; `update` and
/// `post-update` do not, as in Git. Each `*_env` pair list also carries the extra
/// variables given to [`Self::new`] (such as `GIT_SHALLOW_FILE`).
pub(crate) struct ReceiveHookScripts<'a> {
    repo: &'a Repository,
    /// Push options plus the extra variables, for `pre-receive` and `post-receive`.
    receive_env: Vec<(String, String)>,
    /// The extra variables alone, for `update` and `post-update`.
    update_env: Vec<(String, String)>,
}

impl<'a> ReceiveHookScripts<'a> {
    /// Scripts of `repo` for a push that sent `push_options`, with `extra_env` set for every
    /// script.
    pub(crate) fn new(
        repo: &'a Repository,
        push_options: &[String],
        extra_env: Vec<(String, String)>,
    ) -> Self {
        let mut receive_env = Vec::with_capacity(push_options.len() + 1 + extra_env.len());
        receive_env.push((
            "GIT_PUSH_OPTION_COUNT".to_owned(),
            push_options.len().to_string(),
        ));
        receive_env.extend(
            push_options
                .iter()
                .enumerate()
                .map(|(i, opt)| (format!("GIT_PUSH_OPTION_{i}"), opt.clone())),
        );
        receive_env.extend(extra_env.iter().cloned());
        Self {
            repo,
            receive_env,
            update_env: extra_env,
        }
    }

    /// Run `pre-receive` over `commands` (one `<old> <new> <ref>` line each on stdin).
    ///
    /// With `quarantine`, the script also gets its variables so it can read the pushed
    /// objects before they enter the repository.
    pub(crate) fn pre_receive(
        &self,
        commands: &[&RefCommand],
        quarantine: Option<&ReceiveQuarantine>,
    ) -> HookResult {
        let mut env = self.receive_env.clone();
        if let Some(q) = quarantine {
            env.extend(q.hook_env());
        }
        self.run_with_commands("pre-receive", commands, &env)
    }

    /// Run `update` for one command, with `<ref> <old> <new>` as arguments.
    pub(crate) fn update(&self, cmd: &RefCommand) -> HookResult {
        let algo = self.repo.odb().hash_algo();
        let hex = |oid: Option<ObjectId>| oid.unwrap_or_else(|| ObjectId::null(algo)).to_hex();
        let (old, new) = (hex(cmd.old), hex(cmd.new));
        let args = [cmd.refname.as_str(), old.as_str(), new.as_str()];
        run_hook_in_git_dir(self.repo, "update", &args, None, &pairs(&self.update_env))
    }

    /// Run `post-receive` over the applied `commands`.
    pub(crate) fn post_receive(&self, commands: &[&RefCommand]) -> HookResult {
        self.run_with_commands("post-receive", commands, &self.receive_env)
    }

    /// Run `post-update` with the names of the applied `commands` as arguments.
    pub(crate) fn post_update(&self, commands: &[&RefCommand]) -> HookResult {
        let args: Vec<&str> = commands.iter().map(|c| c.refname.as_str()).collect();
        run_hook_in_git_dir(
            self.repo,
            "post-update",
            &args,
            None,
            &pairs(&self.update_env),
        )
    }

    fn run_with_commands(
        &self,
        hook_name: &str,
        commands: &[&RefCommand],
        env: &[(String, String)],
    ) -> HookResult {
        let zero = ObjectId::null(self.repo.odb().hash_algo());
        let stdin: String = commands
            .iter()
            .map(|c| {
                format!(
                    "{} {} {}\n",
                    c.old.unwrap_or(zero).to_hex(),
                    c.new.unwrap_or(zero).to_hex(),
                    c.refname
                )
            })
            .collect();
        run_hook_in_git_dir(
            self.repo,
            hook_name,
            &[],
            Some(stdin.as_bytes()),
            &pairs(env),
        )
    }
}

fn pairs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

fn receive_command_store_update(cmd: &RefCommand) -> crate::refs::store::RefUpdate {
    let mut flags = crate::refs::store::RefUpdateFlags::default();
    if cmd.refname == "HEAD" {
        flags.no_deref = true;
    }
    crate::refs::store::RefUpdate {
        name: cmd.refname.clone(),
        new_value: cmd.new.map(crate::refs::store::RawRef::Direct),
        expected: match cmd.old {
            Some(oid) => crate::refs::store::Expected::Oid(oid),
            None => crate::refs::store::Expected::Missing,
        },
        reflog: None,
        flags,
    }
}

/// Relay what a hook wrote (stdout, then stderr) to the client on sideband 2.
fn relay_hook_sideband(out: &mut Option<&mut dyn Write>, result: &HookResult) -> Result<()> {
    let (Some(w), Some(output)) = (out.as_mut(), result.output()) else {
        return Ok(());
    };
    for chunk in output
        .stdout
        .chunks(65515)
        .chain(output.stderr.chunks(65515))
    {
        crate::pkt_line::write_sideband_packet(*w, 2, chunk).map_err(ServeError::Io)?;
    }
    Ok(())
}

/// Encode the status report as pkt-lines: the unpack result, one line per ref,
/// and a closing flush.
fn build_report(
    unpack_error: Option<&UnpackError>,
    updates: &[RefUpdateResult],
) -> Result<Vec<u8>> {
    let mut report = Vec::new();
    match unpack_error {
        None => write_line(&mut report, "unpack ok")?,
        Some(e) => write_line(&mut report, &format!("unpack {}", one_line(&e.to_string())))?,
    }
    for u in updates {
        match &u.error {
            None => write_line(&mut report, &format!("ok {}", u.refname))?,
            Some(reason) => write_line(
                &mut report,
                &format!("ng {} {}", u.refname, one_line(&reason.to_string())),
            )?,
        }
    }
    write_flush(&mut report)?;
    Ok(report)
}

/// Collapse a message to a single line so it fits in one status pkt-line.
fn one_line(msg: &str) -> String {
    msg.lines().next().unwrap_or_default().trim().to_owned()
}

/// A pushed pack written to a temp file in the quarantine's `pack/` directory.
struct SpooledPack {
    file: tempfile::NamedTempFile,
    /// The 12-byte pack header (signature, version, object count).
    header: [u8; 12],
}

/// Stream exactly one packfile from the push stream into a temp file.
///
/// The pack's end is found by scanning its entries as they arrive, and nothing
/// past the trailing checksum is requested: the push client keeps its write
/// half open until it has read the status report, so waiting for end of input
/// would hang (issue #1656). The checksum itself is verified when the pack is
/// stored.
fn spool_push_pack(
    input: &mut dyn Read,
    odb: &Odb,
    max_bytes: Option<u64>,
    stop: &CancelSignal<'_>,
) -> std::result::Result<SpooledPack, UnpackError> {
    let hash = odb.hash_algo();
    let mut file = tempfile::Builder::new()
        .prefix("tmp_pack_")
        .tempfile_in(odb.objects_dir().join("pack"))?;
    let mut scanner = PackStreamScanner::new(hash);
    let mut header = [0u8; 12];
    let mut chunk = vec![0u8; SPOOL_CHUNK];
    let mut written = 0u64;
    let mut pack_len: Option<u64> = None;
    let mut out = BufWriter::with_capacity(SPOOL_CHUNK, file.as_file_mut());
    loop {
        stop.poll().map_err(|_| UnpackError::Cancelled)?;
        let want = match pack_len {
            Some(len) if written >= len => break,
            Some(len) => usize::try_from(len - written).map_or(chunk.len(), |n| n.min(chunk.len())),
            None => chunk.len(),
        };
        let n = read_some(input, &mut chunk[..want])?;
        if n == 0 {
            return Err(UnpackError::Truncated);
        }
        let mut data = &chunk[..n];
        if pack_len.is_none() {
            scanner.feed(data)?;
            if scanner.is_complete() {
                let len = scanner
                    .pack_body_byte_len()
                    .saturating_add(hash.len() as u64);
                let keep = usize::try_from(len.saturating_sub(written)).map_or(n, |k| k.min(n));
                data = &data[..keep];
                pack_len = Some(len);
            }
        }
        if let Some(limit) = max_bytes {
            let total = pack_len.unwrap_or(written.saturating_add(data.len() as u64));
            if total > limit {
                return Err(UnpackError::TooLarge { limit });
            }
        }
        if let Some(missing) = usize::try_from(written)
            .ok()
            .and_then(|start| header.get_mut(start..))
        {
            let take = missing.len().min(data.len());
            missing[..take].copy_from_slice(&data[..take]);
        }
        out.write_all(data)?;
        written += data.len() as u64;
    }
    out.flush()?;
    drop(out);
    Ok(SpooledPack { file, header })
}

/// `Read::read`, retrying when interrupted by a signal.
fn read_some(input: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match input.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// Check the objects a push stored in the quarantine `odb` against `fsck`.
///
/// `indexed` lists the objects of an installed pack; `None` means the pack was
/// exploded into loose objects, which are then all the quarantine holds.
fn check_received_objects(
    repo: &Repository,
    odb: &Odb,
    indexed: Option<HashSet<ObjectId>>,
    cfg: &ConfigSet,
    fsck: &ObjectFsckPolicy,
) -> std::result::Result<(), UnpackError> {
    let checker = fsck.checker(odb.hash_algo(), repo.git_dir(), repo.environment())?;
    let mut received: Vec<ObjectId> = match indexed {
        Some(ids) => ids.into_iter().collect(),
        None => odb
            .enumerate_local_loose_objects()?
            .into_iter()
            .map(|(oid, _)| oid)
            .collect(),
    };
    received.sort_unstable();
    let parallel = repo
        .thread_policy()
        .try_resolve(ParallelOp::IndexPack, cfg)
        .map_err(crate::error::Error::from)?;
    let verdicts = try_par_map(&received, parallel, received.len(), |oid| {
        if odb.read_info(oid)?.kind == ObjectKind::Blob {
            return Ok(None);
        }
        let obj = odb.read(oid)?;
        Ok(checker
            .check(oid, obj.kind, &obj.data)
            .err()
            .map(|error| RejectedObject {
                oid: *oid,
                kind: obj.kind,
                error,
            }))
    })
    .map_err(|ParallelHashError::Task(e): ParallelHashError<crate::error::Error>| e)?;
    match verdicts.into_iter().flatten().next() {
        Some(rejected) => Err(UnpackError::Rejected(rejected)),
        None => Ok(()),
    }
}

/// Store a spooled push pack: explode small packs into loose objects, index and
/// install larger ones, per `receive.unpacklimit`.
///
/// Returns the ids of the installed pack's objects; `None` when the objects
/// were unpacked as loose files.
///
/// The loose path checks that every pushed object's references resolve,
/// except the parents of `shallow_boundaries`: the pushing client is shallow
/// there and does not have them.
fn ingest_spooled_pack(
    odb: &Odb,
    pack: SpooledPack,
    cfg: &ConfigSet,
    shallow_boundaries: &HashSet<ObjectId>,
) -> std::result::Result<Option<HashSet<ObjectId>>, UnpackError> {
    if should_use_unpack_objects(&pack.header, cfg) {
        let mut reader = BufReader::with_capacity(SPOOL_CHUNK, pack.file.reopen()?);
        unpack_objects(
            &mut reader,
            odb,
            &UnpackOptions {
                quiet: true,
                strict: true,
                shallow_boundaries: shallow_boundaries.clone(),
                ..UnpackOptions::default()
            },
        )?;
        return Ok(None);
    }
    let path = pack.file.into_temp_path();
    let installed = install_pack_path(
        &path,
        odb,
        &IngestPackOptions {
            fix_thin: true,
            // Indexing hashes every object and the trailer is checked while
            // installing, so a second full read of the pack proves nothing.
            skip_post_index_verify: true,
            merged_config: Some(cfg.clone()),
            ..Default::default()
        },
    )?;
    Ok(Some(installed.object_ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lists_unpack_status_then_each_ref() {
        let oid = ObjectId::from_hex("1111111111111111111111111111111111111111")
            .unwrap_or_else(|_| ObjectId::zero());
        let updates = vec![
            RefUpdateResult {
                refname: "refs/heads/main".into(),
                old: None,
                new: Some(oid),
                error: None,
            },
            RefUpdateResult {
                refname: "refs/heads/topic".into(),
                old: Some(oid),
                new: None,
                error: Some(RefUpdateRefusal::DeletionProhibited),
            },
        ];
        let report = build_report(None, &updates).unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&report),
            "000eunpack ok\n0017ok refs/heads/main\n002cng refs/heads/topic deletion prohibited\n0000"
        );
    }

    #[test]
    fn report_renders_typed_unpack_error_and_refusal() {
        let updates = vec![RefUpdateResult {
            refname: "refs/heads/main".into(),
            old: None,
            new: None,
            error: Some(RefUpdateRefusal::UnpackerError),
        }];
        let report = build_report(Some(&UnpackError::Truncated), &updates).unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&report),
            "0029unpack unexpected end of pack stream\n0026ng refs/heads/main unpacker error\n0000"
        );
    }

    #[test]
    fn refusals_render_the_reasons_git_clients_expect() {
        let cases = [
            (RefUpdateRefusal::InvalidRefName, "funny refname"),
            (RefUpdateRefusal::HiddenRef, "deny updating a hidden ref"),
            (RefUpdateRefusal::StaleInfo, "stale info"),
            (RefUpdateRefusal::DeletionProhibited, "deletion prohibited"),
            (
                RefUpdateRefusal::CurrentBranchDeletion,
                "deletion of the current branch prohibited",
            ),
            (
                RefUpdateRefusal::CurrentBranch,
                "branch is currently checked out",
            ),
            (
                RefUpdateRefusal::MissingObjects,
                "missing necessary objects",
            ),
            (RefUpdateRefusal::NonFastForward, "non-fast-forward"),
            (RefUpdateRefusal::BadRef, "bad ref"),
            (
                RefUpdateRefusal::FastForwardCheckFailed,
                "unable to verify fast-forward",
            ),
            (
                RefUpdateRefusal::ShallowUpdateNotAllowed,
                "shallow update not allowed",
            ),
            (RefUpdateRefusal::UnpackerError, "unpacker error"),
            (
                RefUpdateRefusal::AtomicTransactionFailed,
                "atomic transaction failed",
            ),
            (
                RefUpdateRefusal::PreReceiveHookDeclined,
                "pre-receive hook declined",
            ),
            (RefUpdateRefusal::UpdateHookDeclined, "hook declined"),
            (RefUpdateRefusal::Policy("frozen".into()), "frozen"),
            (
                RefUpdateRefusal::TargetBranch {
                    target: "main".into(),
                },
                "main is the target branch; it changes only through integrate",
            ),
            (
                RefUpdateRefusal::RefStore("locked".into()),
                "failed to update ref: locked",
            ),
        ];
        for (refusal, wire) in cases {
            assert_eq!(refusal.to_string(), wire);
        }
    }

    /// Serves `data`, then fails any further read: reading past the pack would
    /// block forever on a real push connection.
    struct NoReadPastEnd<'a> {
        data: &'a [u8],
    }

    impl Read for NoReadPastEnd<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.data.is_empty() {
                return Err(std::io::Error::other("read past the end of the pack"));
            }
            let n = buf.len().min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn sample_pack() -> Vec<u8> {
        use grit_test_support::objects::{HashAlgo as FixtureAlgo, ObjectKind as PK, PackBuilder};
        let mut pb = PackBuilder::new(FixtureAlgo::Sha1);
        let mut seed = 0x2545_f491_u32;
        // Incompressible, so the pack spans several spool chunks.
        let big: Vec<u8> = (0..(SPOOL_CHUNK * 3))
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        pb.add_full(PK::Blob, &big);
        pb.add_full(PK::Blob, b"small\n");
        pb.build().bytes
    }

    fn test_odb(dir: &std::path::Path) -> Odb {
        let objects = dir.join("objects");
        std::fs::create_dir_all(objects.join("pack")).unwrap_or_default();
        Odb::new(&objects)
    }

    #[test]
    fn spool_stops_at_the_trailer_without_reading_further() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let odb = test_odb(tmp.path());
        let pack = sample_pack();
        let mut input = NoReadPastEnd { data: &pack };
        let spooled = spool_push_pack(&mut input, &odb, None, &CancelSignal::never())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::fs::read(spooled.file.path()).unwrap_or_default(), pack);
        assert_eq!(&spooled.header[..], &pack[..12]);
    }

    #[test]
    fn spool_keeps_only_the_pack_when_more_bytes_follow() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let odb = test_odb(tmp.path());
        let pack = sample_pack();
        let mut stream = pack.clone();
        stream.extend_from_slice(b"trailing junk");
        let spooled = spool_push_pack(&mut stream.as_slice(), &odb, None, &CancelSignal::never())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::fs::read(spooled.file.path()).unwrap_or_default(), pack);
    }

    #[test]
    fn spool_reports_typed_errors() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let odb = test_odb(tmp.path());
        let pack = sample_pack();

        let truncated = &pack[..pack.len() - 5];
        let never = CancelSignal::never();
        assert!(matches!(
            spool_push_pack(&mut &truncated[..], &odb, None, &never),
            Err(UnpackError::Truncated)
        ));

        let limit = (pack.len() - 1) as u64;
        assert!(matches!(
            spool_push_pack(&mut pack.as_slice(), &odb, Some(limit), &never),
            Err(UnpackError::TooLarge { limit: l }) if l == limit
        ));
        assert!(
            spool_push_pack(&mut pack.as_slice(), &odb, Some(pack.len() as u64), &never).is_ok()
        );

        assert!(matches!(
            spool_push_pack(&mut &b"JUNK\0\0\0\x02\0\0\0\x01"[..], &odb, None, &never),
            Err(UnpackError::Malformed(PackStreamScanError::BadSignature))
        ));

        let leftovers: Vec<_> = std::fs::read_dir(odb.objects_dir().join("pack"))
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "failed spools must not leave temp files"
        );
    }

    #[test]
    fn deny_current_branch_values() {
        let policy_for = |value: Option<&str>| {
            let mut config = ConfigSet::new();
            if let Some(v) = value {
                let _ = config.add_command_override("receive.denyCurrentBranch", v);
            }
            ReceivePolicy::from_config(&config).deny_current_branch
        };
        assert!(policy_for(None));
        assert!(!policy_for(Some("ignore")));
        assert!(!policy_for(Some("updateInstead")));
        assert!(policy_for(Some("refuse")));
    }

    #[test]
    fn shallow_update_is_off_unless_configured() {
        assert!(!ReceivePolicy::default().shallow_update);
        let mut config = ConfigSet::new();
        let _ = config.add_command_override("receive.shallowUpdate", "true");
        assert!(ReceivePolicy::from_config(&config).shallow_update);
    }

    #[test]
    fn deny_delete_current_values() {
        let policy_for = |value: Option<&str>| {
            let mut config = ConfigSet::new();
            if let Some(v) = value {
                let _ = config.add_command_override("receive.denyDeleteCurrent", v);
            }
            ReceivePolicy::from_config(&config).deny_delete_current
        };
        assert!(policy_for(None));
        assert!(!policy_for(Some("ignore")));
        assert!(!policy_for(Some("warn")));
        assert!(!policy_for(Some("false")));
        // There is no working tree to update for a deleted branch.
        assert!(policy_for(Some("updateInstead")));
        assert!(policy_for(Some("refuse")));
        assert!(ReceivePolicy::default().deny_delete_current);
    }
}
