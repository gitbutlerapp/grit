//! Accepting pushes (`receive-pack`).

use std::collections::HashSet;
use std::io::{Read, Write};

use super::{
    advertised_refs, check_object_format, head_info, parse_oid, read_packet, write_flush,
    write_line, Packet, ProtocolVersion, Result, ServeError, ServeOptions,
};
use crate::config::ConfigSet;
use crate::hooks::{run_hook_in_git_dir, HookResult};
use crate::index_pack::{ingest_received_pack, IngestPackOptions};
use crate::objects::ObjectId;
use crate::receive_pack::{max_input_size_from_config, should_use_unpack_objects};
use crate::receive_quarantine::ReceiveQuarantine;
use crate::repo::Repository;
use crate::unpack_objects::{unpack_objects, UnpackOptions};

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
    /// Refuse updates that would discard commits (`receive.denyNonFastForwards`).
    pub deny_non_fast_forwards: bool,
    /// Refuse ref deletions (`receive.denyDeletes`).
    pub deny_deletes: bool,
    /// In a repository with a working tree, refuse to update or delete the
    /// branch `HEAD` points at (`receive.denyCurrentBranch`).
    pub deny_current_branch: bool,
}

impl Default for ReceivePolicy {
    fn default() -> Self {
        Self {
            deny_non_fast_forwards: false,
            deny_deletes: false,
            deny_current_branch: true,
        }
    }
}

impl ReceivePolicy {
    /// Read the policy from the `receive.*` configuration keys.
    ///
    /// `receive.denyCurrentBranch` accepts Git's values: `refuse`, `true` or
    /// unset refuse the update; `ignore`, `warn`, `false` and `updateInstead`
    /// allow it (this server does not update the working tree).
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
        let deny_current_branch = match config.get("receive.denyCurrentBranch") {
            None => true,
            Some(v) => !matches!(
                v.to_ascii_lowercase().as_str(),
                "ignore" | "warn" | "false" | "no" | "off" | "0" | "updateinstead"
            ),
        };
        Self {
            deny_non_fast_forwards: flag("receive.denyNonFastForwards"),
            deny_deletes: flag("receive.denyDeletes"),
            deny_current_branch,
        }
    }
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
    pub error: Option<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiveOutcome {
    /// Why the pushed pack could not be stored, when it could not.
    pub unpack_error: Option<String>,
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
/// Objects from the pushed pack are stored via [`unpack_objects`] for small
/// packs (below `receive.unpacklimit`) and [`crate::index_pack::ingest_received_pack`]
/// otherwise, matching Git receive-pack. Updates are checked against `policy` and
/// against the value the client expected each ref to have; with the `atomic`
/// capability either every update is applied or none is. Server-side hooks run
/// through the repository [`CommandRunner`](crate::command_runner::CommandRunner)
/// (`pre-receive`, `update`, `post-receive`, `post-update`) with push options and
/// quarantined object ingestion matching Git receive-pack.
///
/// # Parameters
///
/// - `repo`: the repository receiving the push.
/// - `input`: the client's command list and pack.
/// - `output`: where the advertisement and status report are written.
/// - `opts`: framing, protocol version, agent string, and hidden refs. Protocol
///   v2 is not defined for pushes, so it is served as v0.
/// - `policy`: which updates to refuse.
///
/// # Returns
///
/// What was stored and which updates were applied. Refused updates are reported
/// to the client and in the outcome; they are not errors.
///
/// # Errors
///
/// Returns [`ServeError::Protocol`] for a malformed command list and I/O errors
/// from the streams.
pub fn receive_pack(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
    policy: &ReceivePolicy,
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
        repo.odb
            .files_objects_dir()
            .and_then(|d| ReceiveQuarantine::create(d).ok())
    } else {
        None
    };

    let unpack_error = if needs_pack {
        match read_push_pack(input, max_input) {
            Ok(pack) => {
                let ingest_odb = quarantine
                    .as_ref()
                    .map(ReceiveQuarantine::odb)
                    .unwrap_or_else(|| repo.odb.clone());
                ingest_pushed_pack(&ingest_odb, &pack, cfg.as_ref(), max_input)
                    .err()
                    .map(|e| e.to_string())
            }
            Err(msg) => Some(msg),
        }
    } else {
        None
    };

    let mut hook_records = Vec::new();
    let updates = if unpack_error.is_some() {
        request
            .commands
            .iter()
            .map(|c| c.refused("unpacker error"))
            .collect()
    } else {
        apply_commands_with_hooks(
            repo,
            &request,
            opts,
            policy,
            quarantine.as_mut(),
            sideband.then_some(&mut *output),
            &mut hook_records,
        )?
    };

    if request.caps.contains("report-status") || request.caps.contains("report-status-v2") {
        let report = build_report(unpack_error.as_deref(), &updates)?;
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
    caps.push(format!("object-format={}", repo.odb.hash_algo().name()));
    if !opts.agent.is_empty() {
        caps.push(format!("agent={}", opts.agent));
    }
    let caps = caps.join(" ");

    let refs = advertised_refs(repo, &opts.hidden_refs)?;
    if refs.is_empty() {
        let zero = ObjectId::null(repo.odb.hash_algo());
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

/// One `<old> <new> <ref>` line from the client.
struct Command {
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    refname: String,
}

impl Command {
    fn refused(&self, reason: &str) -> RefUpdateResult {
        RefUpdateResult {
            refname: self.refname.clone(),
            old: self.old,
            new: self.new,
            error: Some(reason.to_owned()),
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

/// The command list of a push.
struct PushRequest {
    commands: Vec<Command>,
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
                commands.push(Command {
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
                    let Some(value) = line.strip_prefix("push-option ") else {
                        return Err(ServeError::Protocol(format!(
                            "expected push-option line, got '{line}'"
                        )));
                    };
                    push_options.push(value.to_owned());
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
fn apply_commands_with_hooks(
    repo: &Repository,
    request: &PushRequest,
    opts: &ServeOptions,
    policy: &ReceivePolicy,
    quarantine: Option<&mut ReceiveQuarantine>,
    mut sideband_out: Option<&mut dyn Write>,
    hook_records: &mut Vec<HookRunRecord>,
) -> Result<Vec<RefUpdateResult>> {
    let lookup_odb = quarantine.as_ref().map(|q| q.odb());
    let current_branch = if repo.is_bare() {
        None
    } else {
        head_info(repo).target
    };
    let mut results: Vec<RefUpdateResult> = request
        .commands
        .iter()
        .map(|c| {
            match check_command(
                repo,
                lookup_odb.as_ref(),
                c,
                opts,
                policy,
                current_branch.as_deref(),
            ) {
                Some(reason) => c.refused(reason),
                None => c.accepted(),
            }
        })
        .collect();

    if request.caps.contains("atomic") && results.iter().any(|r| r.error.is_some()) {
        for r in results.iter_mut().filter(|r| r.error.is_none()) {
            r.error = Some("atomic transaction failed".to_owned());
        }
        return Ok(results);
    }

    let mut hook_env_owned = push_option_env_owned(&request.push_options);
    if let Some(q) = quarantine.as_ref() {
        hook_env_owned.extend(q.hook_env());
    }
    let hook_env: Vec<(&str, &str)> = hook_env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let all_cmds: Vec<&Command> = request.commands.iter().collect();
    let pre_stdin = commands_stdin(&all_cmds);
    let (pre_result, pre_out) = run_hook_in_git_dir(
        repo,
        "pre-receive",
        &[],
        Some(pre_stdin.as_bytes()),
        &hook_env,
    );
    relay_hook_sideband(&mut sideband_out, &pre_out)?;
    hook_records.push(hook_record("pre-receive", &pre_result));
    if matches!(pre_result, HookResult::Failed(_)) {
        for r in results.iter_mut().filter(|r| r.error.is_none()) {
            r.error = Some("pre-receive hook declined".to_owned());
        }
        return Ok(results);
    }

    if results.iter().all(|r| r.error.is_some()) {
        return Ok(results);
    }

    if let Some(q) = quarantine {
        q.migrate().map_err(ServeError::Repository)?;
        repo.odb.invalidate_packs();
    }

    for (cmd, result) in request.commands.iter().zip(results.iter_mut()) {
        if result.error.is_some() {
            continue;
        }
        let zero = null_oid_hex();
        let old_hex = cmd.old.map(|o| o.to_hex()).unwrap_or_else(|| zero.clone());
        let new_hex = cmd.new.map(|o| o.to_hex()).unwrap_or(zero.clone());
        let args = [cmd.refname.as_str(), old_hex.as_str(), new_hex.as_str()];
        let (update_result, update_out) =
            run_hook_in_git_dir(repo, "update", &args, None, &hook_env);
        relay_hook_sideband(&mut sideband_out, &update_out)?;
        hook_records.push(hook_record("update", &update_result));
        if matches!(update_result, HookResult::Failed(_)) {
            result.error = Some("hook declined".to_owned());
        }
    }

    if request.caps.contains("atomic") && results.iter().any(|r| r.error.is_some()) {
        for r in results.iter_mut().filter(|r| r.error.is_none()) {
            r.error = Some("atomic transaction failed".to_owned());
        }
        return Ok(results);
    }

    for result in results.iter_mut().filter(|r| r.error.is_none()) {
        let applied = match result.new {
            Some(new) => crate::refs::write_ref(&repo.git_dir, &result.refname, &new),
            None => crate::refs::delete_ref(&repo.git_dir, &result.refname),
        };
        if let Err(e) = applied {
            result.error = Some(format!("failed to update ref: {e}"));
        }
    }

    let successful: Vec<&Command> = request
        .commands
        .iter()
        .zip(results.iter())
        .filter(|(_, r)| r.error.is_none())
        .map(|(c, _)| c)
        .collect();
    let post_stdin = commands_stdin(&successful);
    let (post_result, post_out) = run_hook_in_git_dir(
        repo,
        "post-receive",
        &[],
        Some(post_stdin.as_bytes()),
        &hook_env,
    );
    relay_hook_sideband(&mut sideband_out, &post_out)?;
    hook_records.push(hook_record("post-receive", &post_result));

    let mut post_update_args: Vec<&str> = Vec::new();
    for result in results.iter().filter(|r| r.error.is_none()) {
        post_update_args.push(result.refname.as_str());
    }
    if !post_update_args.is_empty() {
        let (pu_result, pu_out) =
            run_hook_in_git_dir(repo, "post-update", &post_update_args, None, &hook_env);
        relay_hook_sideband(&mut sideband_out, &pu_out)?;
        hook_records.push(hook_record("post-update", &pu_result));
    }

    Ok(results)
}

fn hook_record(name: &str, result: &HookResult) -> HookRunRecord {
    HookRunRecord {
        name: name.to_owned(),
        ran: result.was_executed(),
        exit_code: match result {
            HookResult::Failed(code) => Some(*code),
            HookResult::Success => Some(0),
            HookResult::NotFound => None,
        },
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

fn commands_stdin(commands: &[&Command]) -> String {
    let zero = null_oid_hex();
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

fn null_oid_hex() -> String {
    "0000000000000000000000000000000000000000".to_owned()
}

fn relay_hook_sideband(out: &mut Option<&mut dyn Write>, captured: &[u8]) -> Result<()> {
    let Some(w) = out.as_mut() else {
        return Ok(());
    };
    if captured.is_empty() {
        return Ok(());
    }
    for chunk in captured.chunks(65515) {
        crate::pkt_line::write_sideband_packet(*w, 2, chunk).map_err(ServeError::Io)?;
    }
    Ok(())
}

/// Decide whether one command may be applied. Returns the refusal reason.
fn check_command(
    repo: &Repository,
    lookup: Option<&crate::odb::Odb>,
    cmd: &Command,
    opts: &ServeOptions,
    policy: &ReceivePolicy,
    current_branch: Option<&str>,
) -> Option<&'static str> {
    if !cmd.refname.starts_with("refs/") || !crate::refs::is_valid_storable_ref_name(&cmd.refname) {
        return Some("funny refname");
    }
    if crate::hide_refs::ref_is_hidden(&cmd.refname, &cmd.refname, &opts.hidden_refs) {
        return Some("deny updating a hidden ref");
    }
    let current = crate::refs::resolve_ref(&repo.git_dir, &cmd.refname).ok();
    if current != cmd.old {
        return Some("stale info");
    }
    let is_current_branch = current_branch == Some(cmd.refname.as_str());

    let Some(new) = cmd.new else {
        if policy.deny_deletes {
            return Some("deletion prohibited");
        }
        if is_current_branch && policy.deny_current_branch {
            return Some("deletion of the current branch prohibited");
        }
        return None;
    };
    let odb = lookup.unwrap_or(&repo.odb);
    if !odb.exists(&new) {
        return Some("missing necessary objects");
    }
    if is_current_branch && policy.deny_current_branch {
        return Some("branch is currently checked out");
    }
    if let (Some(old), true) = (current, policy.deny_non_fast_forwards) {
        let fast_forward = crate::merge_base::is_ancestor(repo, old, new).unwrap_or(false);
        if !fast_forward {
            return Some("non-fast-forward");
        }
    }
    None
}

/// Encode the status report as pkt-lines: the unpack result, one line per ref,
/// and a closing flush.
fn build_report(unpack_error: Option<&str>, updates: &[RefUpdateResult]) -> Result<Vec<u8>> {
    let mut report = Vec::new();
    match unpack_error {
        None => write_line(&mut report, "unpack ok")?,
        Some(e) => write_line(&mut report, &format!("unpack {}", one_line(e)))?,
    }
    for u in updates {
        match &u.error {
            None => write_line(&mut report, &format!("ok {}", u.refname))?,
            Some(reason) => write_line(
                &mut report,
                &format!("ng {} {}", u.refname, one_line(reason)),
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

/// Read the pack bytes that follow the ref-update command list on the wire.
fn read_push_pack(
    input: &mut dyn Read,
    max_bytes: Option<u64>,
) -> std::result::Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = input.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if let Some(limit) = max_bytes {
            let next = buf.len().saturating_add(n);
            if next as u64 > limit {
                return Err("pack exceeds maximum input size".to_owned());
            }
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

/// Store a pushed pack using unpack-objects or index-pack, per Git limits.
fn ingest_pushed_pack(
    odb: &crate::odb::Odb,
    pack: &[u8],
    cfg: &ConfigSet,
    max_input: Option<u64>,
) -> Result<()> {
    if should_use_unpack_objects(pack, cfg) {
        let mut cursor = std::io::Cursor::new(pack);
        unpack_objects(
            &mut cursor,
            odb,
            &UnpackOptions {
                quiet: true,
                strict: true,
                max_input_bytes: max_input,
                ..UnpackOptions::default()
            },
        )?;
    } else {
        ingest_received_pack(
            pack.to_vec(),
            odb,
            &IngestPackOptions {
                fix_thin: true,
                ..Default::default()
            },
        )?;
    }
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
                error: Some("deletion prohibited".into()),
            },
        ];
        let report = build_report(None, &updates).unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&report),
            "000eunpack ok\n0017ok refs/heads/main\n002cng refs/heads/topic deletion prohibited\n0000"
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
}
