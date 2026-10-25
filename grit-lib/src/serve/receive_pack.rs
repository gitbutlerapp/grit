//! Accepting pushes (`receive-pack`).

use std::collections::HashSet;
use std::io::{BufReader, BufWriter, Read, Write};

use super::{
    advertised_refs, check_object_format, parse_oid, read_packet, ref_update_policy, write_flush,
    write_line, Packet, Peel, ProtocolVersion, Result, ServeError, ServeOptions,
};
use crate::config::ConfigSet;
use crate::hooks::{run_hook_in_git_dir, HookResult};
use crate::index_pack::{install_pack_path, IngestPackOptions};
use crate::objects::pack_stream_scan::{PackStreamScanError, PackStreamScanner};
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::receive_pack::{max_input_size_from_config, should_use_unpack_objects};
use crate::receive_quarantine::ReceiveQuarantine;
use crate::repo::Repository;
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

/// Which ref updates the server refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl Default for ReceivePolicy {
    fn default() -> Self {
        Self {
            deny_non_fast_forwards: false,
            deny_deletes: false,
            deny_current_branch: true,
            deny_delete_current: true,
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
        Self {
            deny_non_fast_forwards: flag("receive.denyNonFastForwards"),
            deny_deletes: flag("receive.denyDeletes"),
            deny_current_branch: !allows("receive.denyCurrentBranch", true),
            deny_delete_current: !allows("receive.denyDeleteCurrent", false),
        }
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
    /// Reading the client stream or writing the spooled pack failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
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

    let unpack_error = match quarantine.as_ref() {
        Some(q) => {
            let odb = q.odb();
            spool_push_pack(input, &odb, max_input)
                .and_then(|pack| ingest_spooled_pack(&odb, pack, cfg.as_ref()))
                .err()
        }
        None => None,
    };

    let mut hook_records = Vec::new();
    let updates = if unpack_error.is_some() {
        request
            .commands
            .iter()
            .map(|c| c.refused(RefUpdateRefusal::UnpackerError))
            .collect()
    } else {
        apply_commands_with_hooks(
            repo,
            &request,
            opts,
            policy,
            hook,
            quarantine.as_mut(),
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
}

/// Read the command list up to its flush. Returns `None` when the client sent
/// no commands (nothing to push).
fn read_commands(repo: &Repository, input: &mut dyn Read) -> Result<Option<PushRequest>> {
    let mut commands = Vec::new();
    let mut caps = HashSet::new();
    loop {
        match read_packet(input)? {
            None | Some(Packet::Flush) => break,
            Some(Packet::Data(line)) => {
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
    }))
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
        .map(|c| {
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
    if !accepted.is_empty() {
        if let Err(refusal) = hook.pre_receive(&accepted, lookup_odb.as_ref().unwrap_or(repo.odb()))
        {
            refuse_remaining(&mut results, &refusal);
            return Ok(results);
        }
    }

    let algo = repo.odb().hash_algo();
    let push_option_env = push_option_env_owned(&request.push_options);
    let mut pre_hook_env_owned = push_option_env.clone();
    if let Some(q) = quarantine.as_ref() {
        pre_hook_env_owned.extend(q.hook_env());
    }
    let pre_hook_env: Vec<(&str, &str)> = pre_hook_env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let update_hook_env: &[(&str, &str)] = &[];
    let post_receive_env_owned = push_option_env;
    let post_receive_env: Vec<(&str, &str)> = post_receive_env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let all_cmds: Vec<&RefCommand> = request.commands.iter().collect();
    let pre_stdin = commands_stdin(&all_cmds, algo);
    let pre_result = run_hook_in_git_dir(
        repo,
        "pre-receive",
        &[],
        Some(pre_stdin.as_bytes()),
        &pre_hook_env,
    );
    relay_hook_sideband(&mut sideband_out, &pre_result)?;
    hook_records.push(hook_record("pre-receive", &pre_result));
    if !pre_result.is_ok() {
        refuse_remaining(&mut results, &RefUpdateRefusal::PreReceiveHookDeclined);
        return Ok(results);
    }

    if results.iter().all(|r| r.error.is_some()) {
        return Ok(results);
    }

    if let Some(q) = quarantine {
        q.migrate().map_err(ServeError::Repository)?;
        repo.odb().invalidate_packs();
    }

    for (cmd, result) in request.commands.iter().zip(results.iter_mut()) {
        if result.error.is_some() {
            continue;
        }
        if let Err(refusal) = hook.update(cmd, repo.odb()) {
            result.error = Some(refusal);
            continue;
        }
        let zero = null_oid_hex(algo);
        let old_hex = cmd.old.map(|o| o.to_hex()).unwrap_or_else(|| zero.clone());
        let new_hex = cmd.new.map(|o| o.to_hex()).unwrap_or(zero.clone());
        let args = [cmd.refname.as_str(), old_hex.as_str(), new_hex.as_str()];
        let update_result = run_hook_in_git_dir(repo, "update", &args, None, update_hook_env);
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
        if let Err(e) = crate::refs::commit_ref_store_batch(repo.git_dir(), &batch) {
            let refusal = RefUpdateRefusal::RefStore(e.to_string());
            for (idx, _) in pending {
                if results[idx].error.is_none() {
                    results[idx].error = Some(refusal.clone());
                }
            }
        }
    } else {
        for (idx, update) in pending {
            if let Err(e) = crate::refs::commit_store_update(repo.git_dir(), update) {
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
        let post_stdin = commands_stdin(&successful, algo);
        let post_result = run_hook_in_git_dir(
            repo,
            "post-receive",
            &[],
            Some(post_stdin.as_bytes()),
            &post_receive_env,
        );
        relay_hook_sideband(&mut sideband_out, &post_result)?;
        hook_records.push(hook_record("post-receive", &post_result));

        let mut post_update_args: Vec<&str> = Vec::new();
        for result in results.iter().filter(|r| r.error.is_none()) {
            post_update_args.push(result.refname.as_str());
        }
        let pu_result = run_hook_in_git_dir(
            repo,
            "post-update",
            &post_update_args,
            None,
            update_hook_env,
        );
        relay_hook_sideband(&mut sideband_out, &pu_result)?;
        hook_records.push(hook_record("post-update", &pu_result));
    }

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

fn push_option_env_owned(options: &[String]) -> Vec<(String, String)> {
    let mut pairs = vec![(
        "GIT_PUSH_OPTION_COUNT".to_owned(),
        options.len().to_string(),
    )];
    for (i, opt) in options.iter().enumerate() {
        pairs.push((format!("GIT_PUSH_OPTION_{i}"), opt.clone()));
    }
    pairs
}

fn commands_stdin(commands: &[&RefCommand], algo: crate::objects::HashAlgo) -> String {
    let zero = null_oid_hex(algo);
    commands
        .iter()
        .map(|c| {
            format!(
                "{} {} {}\n",
                c.old.map(|o| o.to_hex()).unwrap_or_else(|| zero.clone()),
                c.new.map(|o| o.to_hex()).unwrap_or_else(|| zero.clone()),
                c.refname
            )
        })
        .collect()
}

fn null_oid_hex(algo: crate::objects::HashAlgo) -> String {
    ObjectId::null(algo).to_hex()
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

/// Store a spooled push pack: explode small packs into loose objects, index and
/// install larger ones, per `receive.unpacklimit`.
fn ingest_spooled_pack(
    odb: &Odb,
    pack: SpooledPack,
    cfg: &ConfigSet,
) -> std::result::Result<(), UnpackError> {
    if should_use_unpack_objects(&pack.header, cfg) {
        let mut reader = BufReader::with_capacity(SPOOL_CHUNK, pack.file.reopen()?);
        unpack_objects(
            &mut reader,
            odb,
            &UnpackOptions {
                quiet: true,
                strict: true,
                ..UnpackOptions::default()
            },
        )?;
        return Ok(());
    }
    let path = pack.file.into_temp_path();
    install_pack_path(
        &path,
        odb,
        &IngestPackOptions {
            fix_thin: true,
            // Indexing hashes every object and the trailer is checked while
            // installing, so a second full read of the pack proves nothing.
            skip_post_index_verify: true,
            ..Default::default()
        },
    )?;
    Ok(())
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
        let spooled = spool_push_pack(&mut input, &odb, None).unwrap_or_else(|e| panic!("{e}"));
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
        let spooled =
            spool_push_pack(&mut stream.as_slice(), &odb, None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::fs::read(spooled.file.path()).unwrap_or_default(), pack);
    }

    #[test]
    fn spool_reports_typed_errors() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let odb = test_odb(tmp.path());
        let pack = sample_pack();

        let truncated = &pack[..pack.len() - 5];
        assert!(matches!(
            spool_push_pack(&mut &truncated[..], &odb, None),
            Err(UnpackError::Truncated)
        ));

        let limit = (pack.len() - 1) as u64;
        assert!(matches!(
            spool_push_pack(&mut pack.as_slice(), &odb, Some(limit)),
            Err(UnpackError::TooLarge { limit: l }) if l == limit
        ));
        assert!(spool_push_pack(&mut pack.as_slice(), &odb, Some(pack.len() as u64)).is_ok());

        assert!(matches!(
            spool_push_pack(&mut &b"JUNK\0\0\0\x02\0\0\0\x01"[..], &odb, None),
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
