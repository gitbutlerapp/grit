//! `grit remote` — manage remote repository connections.
//!
//! Matches Git's `git remote` porcelain: list/show/add/remove/rename/set-head/prune/update,
//! URL helpers, and ref-aware removal/rename (including reflog rename messages).

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::ls_remote::{ls_remote, Options as LsRemoteOpts};
use grit_lib::merge_base::is_ancestor;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::refs;
use grit_lib::repo::Repository;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::commands::fetch::{
    map_ref_through_refspecs, ref_excluded_by_fetch_refspecs, FetchRefspec,
};
use crate::explicit_exit::ExplicitExit;

/// Clap surface for `--git-completion-helper` (full argv parsing is manual, like Git).
#[derive(Debug, ClapArgs)]
#[command(name = "remote", about = "Manage set of tracked repositories")]
pub struct Args {
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

/// Entry point from `main` after global options and `remote` subcommand name are stripped.
pub fn run_from_argv(rest: &[String]) -> Result<()> {
    crate::commands::upstream_synopsis_help::try_print_upstream_help_and_exit("remote", rest);
    let (verbose, rest) = consume_verbose_prefix(rest);
    if rest.is_empty() {
        return cmd_list(verbose);
    }
    match rest[0].as_str() {
        "add" => cmd_add(&rest[1..]),
        "rename" => cmd_rename(&rest[1..], false),
        "rm" | "remove" => cmd_remove(&rest[1..]),
        "set-head" => cmd_set_head(&rest[1..]),
        "set-branches" => cmd_set_branches(&rest[1..]),
        "get-url" => cmd_get_url(&rest[1..]),
        "set-url" => cmd_set_url(&rest[1..]),
        "show" => cmd_show(&rest[1..], verbose),
        "prune" => cmd_prune(&rest[1..]),
        "update" => cmd_update(&rest[1..], verbose),
        s if s.starts_with('-') => bail!("unknown option: {s}"),
        other => {
            eprintln!("error: unknown subcommand: `{other}`");
            print_remote_usage_fallback();
            Err(anyhow::Error::new(ExplicitExit {
                code: 129,
                message: String::new(),
            }))
        }
    }
}

fn consume_verbose_prefix(rest: &[String]) -> (bool, &[String]) {
    let mut v = false;
    let mut i = 0;
    while i < rest.len() && (rest[i] == "-v" || rest[i] == "--verbose") {
        v = true;
        i += 1;
    }
    (v, &rest[i..])
}

fn remote_usage_lines() -> &'static str {
    "usage: git remote [-v | --verbose]\n\
     or: git remote add [-t <branch>] [-m <master>] [-f] [--tags | --no-tags] [--mirror=<fetch|push>] <name> <URL>\n\
     or: git remote rename [--[no-]progress] <old> <new>\n\
     or: git remote remove <name>\n\
     or: git remote set-head <name> (-a | --auto | -d | --delete | <branch>)\n\
     or: git remote set-branches [--add] <name> <branch>...\n\
     or: git remote get-url [--push] [--all] <name>\n\
     or: git remote set-url [--push] <name> <newurl> [<oldurl>]\n\
     or: git remote set-url --add [--push] <name> <newurl>\n\
     or: git remote set-url --delete [--push] <name> <url>\n\
     or: git remote [-v | --verbose] show [-n] <name>...\n\
     or: git remote prune [-n | --dry-run] <name>...\n\
     or: git remote [-v | --verbose] update [-p | --prune] [(<group> | <remote>)...]"
}

fn print_remote_usage_fallback() {
    println!("{}", remote_usage_lines());
}

/// Build the `usage: ...` / `   or: ...` block for a subcommand and return it as an `ExplicitExit`
/// with code 129, matching Git's `usage_with_options` (printed to stderr, no `error:` prefix).
fn usage_exit(lines: &[&str]) -> anyhow::Error {
    let mut msg = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            msg.push_str(&format!("usage: {l}"));
        } else {
            msg.push_str(&format!("\n    or: {l}"));
        }
    }
    anyhow::Error::new(ExplicitExit {
        code: 129,
        message: msg,
    })
}

fn valid_remote_name(name: &str) -> bool {
    let probe = format!("refs/remotes/{name}/test");
    check_refname_format(&probe, &RefNameOptions::default()).is_ok()
}

fn resolve_git_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("GIT_DIR") {
        let cwd = std::env::current_dir().context("cannot determine current directory")?;
        let mut p = PathBuf::from(dir);
        if p.is_relative() {
            p = cwd.join(p);
        }
        return grit_lib::repo::resolve_git_directory_arg(&p).map_err(|e| anyhow::anyhow!(e));
    }
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let mut cur = cwd.as_path();
    loop {
        let dot_git = cur.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git);
        }
        if dot_git.is_file() {
            if let Ok(content) = std::fs::read_to_string(&dot_git) {
                for line in content.lines() {
                    if let Some(rest) = line.strip_prefix("gitdir:") {
                        let path = rest.trim();
                        let resolved = if Path::new(path).is_absolute() {
                            PathBuf::from(path)
                        } else {
                            cur.join(path)
                        };
                        return Ok(resolved);
                    }
                }
            }
        }
        if cur.join("objects").is_dir() && cur.join("HEAD").is_file() {
            return Ok(cur.to_path_buf());
        }
        cur = match cur.parent() {
            Some(p) => p,
            None => bail!("not a git repository (or any of the parent directories): .git"),
        };
    }
}

fn load_local_config(git_dir: &Path) -> Result<ConfigSet> {
    Ok(ConfigSet::load(Some(git_dir), true)?)
}

fn load_or_create_config_file(config_path: &Path) -> Result<ConfigFile> {
    match ConfigFile::from_path(config_path, ConfigScope::Local)? {
        Some(cfg) => Ok(cfg),
        None => Ok(ConfigFile::parse(config_path, "", ConfigScope::Local)?),
    }
}

/// Write `config_file`, but first reject the write if `<config>.lock` already exists, mirroring
/// Git's config locking (`git remote set-url` with a stale lock must fail without clobbering).
fn write_config_respecting_lock(config_file: &ConfigFile, config_path: &Path) -> Result<()> {
    let lock = config_path.with_extension("lock");
    if lock.exists() {
        bail!("could not lock config file {}", config_path.display());
    }
    config_file.write().context("writing config")
}

fn find_git_dir(path: &Path) -> Result<PathBuf> {
    if path.join("objects").is_dir() && path.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        return Ok(dot_git);
    }
    bail!("not a git repository: '{}'", path.display())
}

fn apply_url_instead_of(config: &ConfigSet, url: &str) -> String {
    let mut best: Option<(usize, String)> = None;
    for e in config.entries() {
        let k = e.key.as_str();
        if !k.starts_with("url.") || !k.ends_with(".insteadof") {
            continue;
        }
        let Some(long) = k
            .strip_prefix("url.")
            .and_then(|s| s.strip_suffix(".insteadof"))
        else {
            continue;
        };
        let Some(short) = e.value.as_deref() else {
            continue;
        };
        if url == short {
            let len = long.len();
            if best.as_ref().is_none_or(|(l, _)| len > *l) {
                best = Some((len, long.to_owned()));
            }
        }
    }
    best.map(|(_, u)| u).unwrap_or_else(|| url.to_owned())
}

fn remote_names_sorted(config: &ConfigSet) -> Vec<String> {
    let mut names: Vec<String> = collect_remote_names_from_config(config);
    names.sort();
    names
}

/// Names with any `remote.<name>.*` config (URL, `vcs`, fetch, etc.), matching Git's `remote_get`.
fn collect_remote_names_from_config(config: &ConfigSet) -> Vec<String> {
    let mut seen = HashSet::new();
    for e in config.entries() {
        let parts: Vec<&str> = e.key.splitn(3, '.').collect();
        if parts.len() == 3 && parts[0] == "remote" {
            seen.insert(parts[1].to_owned());
        }
    }
    seen.into_iter().collect()
}

fn remote_section_exists(config: &ConfigSet, name: &str) -> bool {
    let prefix = format!("remote.{name}.");
    config.entries().iter().any(|e| e.key.starts_with(&prefix))
}

/// Whether a remote named `name` is configured in the **repository-level** config file (Git's
/// `remote_is_configured(..., in_repo=1)`), ignoring global/system config.
fn remote_configured_in_repo(git_dir: &Path, name: &str) -> Result<bool> {
    let config_path = git_dir.join("config");
    let Some(cfg) = ConfigFile::from_path(&config_path, ConfigScope::Local)? else {
        return Ok(false);
    };
    let prefix = format!("remote.{name}.");
    Ok(cfg.entries.iter().any(|e| e.key.starts_with(&prefix)))
}

fn check_remote_name_collision(config: &ConfigSet, name: &str) -> Result<()> {
    for other in collect_remote_names_from_config(config) {
        if other == name {
            continue;
        }
        if name.starts_with(&format!("{other}/")) {
            bail!(
                "remote name '{}' is a subset of existing remote '{}'",
                name,
                other
            );
        }
        if other.starts_with(&format!("{name}/")) {
            bail!(
                "remote name '{}' is a superset of existing remote '{}'",
                name,
                other
            );
        }
    }
    Ok(())
}

struct RemoteUrls {
    fetch: Vec<String>,
    push: Vec<String>,
}

/// Collect a remote's URL list applying Git's `add_url` semantics: an empty value clears the
/// accumulated list (so later non-empty values start a fresh list), matching
/// `git -c remote.<n>.url= -c remote.<n>.url=real`.
fn collect_url_list(config: &ConfigSet, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in config.get_all(key) {
        if v.is_empty() {
            out.clear();
        } else {
            out.push(v);
        }
    }
    out
}

fn remote_urls_effective(config: &ConfigSet, name: &str) -> Option<RemoteUrls> {
    let fetch = collect_url_list(config, &format!("remote.{name}.url"));
    if fetch.is_empty() {
        return None;
    }
    let push = collect_url_list(config, &format!("remote.{name}.pushurl"));
    Some(RemoteUrls { fetch, push })
}

fn mirror_flags(config: &ConfigSet, name: &str) -> (bool, bool) {
    let fetch_lines = config.get_all(&format!("remote.{name}.fetch"));
    let fetch_mirror = fetch_lines.iter().any(|line| {
        let t = line.trim();
        t == "+refs/*:refs/*" || t.contains("+refs/*:refs/*")
    });
    let mirror_true = config
        .get(&format!("remote.{name}.mirror"))
        .map(|s| s.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // `remote.<n>.mirror=true` with the full mirror fetch refspec is `git remote add --mirror`;
    // push-only mirror sets `mirror` without the `+refs/*:refs/*` fetch line.
    let push_mirror = mirror_true && !fetch_mirror;
    (fetch_mirror, push_mirror)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagsMode {
    Unset,
    Default,
    All,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MirrorOpt {
    None,
    Both,
    Fetch,
    Push,
}

fn cmd_add(rest: &[String]) -> Result<()> {
    let git_dir = resolve_git_dir()?;
    let config_path = git_dir.join("config");
    let config = load_local_config(&git_dir)?;

    let mut fetch_immediate = false;
    let mut tags = TagsMode::Unset;
    let mut mirror = MirrorOpt::None;
    let mut track: Vec<String> = Vec::new();
    let mut master: Option<String> = None;
    let mut i = 0usize;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "-f" => {
                fetch_immediate = true;
                i += 1;
            }
            "--tags" => {
                tags = TagsMode::All;
                i += 1;
            }
            "--no-tags" => {
                tags = TagsMode::None;
                i += 1;
            }
            "--mirror" => {
                mirror = MirrorOpt::Both;
                i += 1;
            }
            "-t" | "--track" => {
                i += 1;
                if i >= rest.len() {
                    bail!("option requires an argument: {a}");
                }
                track.push(rest[i].clone());
                i += 1;
            }
            "-m" | "--master" => {
                i += 1;
                if i >= rest.len() {
                    bail!("option requires an argument: {a}");
                }
                master = Some(rest[i].clone());
                i += 1;
            }
            _ if a.starts_with("--mirror=") => {
                let v = a.strip_prefix("--mirror=").unwrap_or("");
                mirror = match v {
                    "fetch" => MirrorOpt::Fetch,
                    "push" => MirrorOpt::Push,
                    other => bail!("unknown --mirror argument: {other}"),
                };
                i += 1;
            }
            _ if a.starts_with('-') => bail!("unknown option: {a}"),
            _ => break,
        }
    }
    // Mirror Git's argument validation in `add`.
    if !matches!(mirror, MirrorOpt::None) && master.is_some() {
        bail!("specifying a master branch makes no sense with --mirror");
    }
    if matches!(mirror, MirrorOpt::Push) && !track.is_empty() {
        bail!("specifying branches to track makes sense only with fetch mirrors");
    }
    let rest = &rest[i..];
    if rest.len() != 2 {
        return Err(usage_exit(&["git remote add [<options>] <name> <url>"]));
    }
    let name = rest[0].clone();
    let mut url = rest[1].clone();
    if !valid_remote_name(&name) {
        bail!("fatal: '{}' is not a valid remote name", name);
    }
    check_remote_name_collision(&config, &name)?;
    if remote_section_exists(&config, &name) {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 3,
            message: format!("error: remote {name} already exists."),
        }));
    }

    url = apply_url_instead_of(&config, &url);

    let mut config_file = load_or_create_config_file(&config_path)?;

    // Mirror Git's `add`: a fetch refspec is written for everything except a push-only mirror; the
    // `mirror=true` flag is written for push and both. Track branches default to "*".
    if track.is_empty() {
        track.push("*".to_owned());
    }
    let write_fetch = matches!(mirror, MirrorOpt::None | MirrorOpt::Both | MirrorOpt::Fetch);
    let write_mirror_flag = matches!(mirror, MirrorOpt::Both | MirrorOpt::Push);
    // A fetch mirror writes `+refs/<b>:refs/<b>` rather than remote-tracking destinations.
    let mirror_fetch = matches!(mirror, MirrorOpt::Both | MirrorOpt::Fetch);

    config_file.set(&format!("remote.{name}.url"), &url)?;
    let fetch_key = format!("remote.{name}.fetch");
    if write_fetch {
        for (idx, b) in track.iter().enumerate() {
            // `add_branch`: mirror -> `+refs/<b>:refs/<b>`, else `+refs/heads/<b>:refs/remotes/<n>/<b>`.
            let spec = if mirror_fetch {
                format!("+refs/{b}:refs/{b}")
            } else {
                format!("+refs/heads/{b}:refs/remotes/{name}/{b}")
            };
            if idx == 0 {
                config_file.set(&fetch_key, &spec)?;
            } else {
                config_file.add_value(&fetch_key, &spec)?;
            }
        }
    }
    if write_mirror_flag {
        config_file.set(&format!("remote.{name}.mirror"), "true")?;
    }
    match tags {
        // Git writes the camelCase `tagOpt` variable name (t5505 greps for `tagOpt`).
        TagsMode::All => {
            config_file.set(&format!("remote.{name}.tagOpt"), "--tags")?;
        }
        TagsMode::None => {
            config_file.set(&format!("remote.{name}.tagOpt"), "--no-tags")?;
        }
        _ => {}
    }
    config_file.write().context("writing config")?;

    if let Some(ref m) = master {
        let head_ref = format!("refs/remotes/{name}/HEAD");
        let target = format!("refs/remotes/{name}/{m}");
        refs::write_symbolic_ref(&git_dir, &head_ref, &target)
            .with_context(|| format!("Could not setup master '{m}'"))?;
    }

    if fetch_immediate {
        let self_exe = std::env::current_exe().context("cannot determine own executable")?;
        let status = std::process::Command::new(&self_exe)
            .arg("fetch")
            .arg(&name)
            .status()
            .context("failed to fetch")?;
        if !status.success() {
            bail!("fetch from {name} failed");
        }
    }

    Ok(())
}

fn cmd_list(verbose: bool) -> Result<()> {
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    for name in remote_names_sorted(&config) {
        if !verbose {
            println!("{name}");
            continue;
        }
        let Some(urls) = remote_urls_effective(&config, &name) else {
            println!("{name}");
            continue;
        };
        let fetch0 = urls.fetch.first().cloned().unwrap_or_default();
        let promisor = config
            .get(&format!("remote.{name}.partialclonefilter"))
            .unwrap_or_default();
        let mut line = format!("{name}\t{fetch0} (fetch)");
        if !promisor.is_empty() {
            line.push_str(&format!(" [{promisor}]"));
        }
        println!("{line}");
        let push_urls: Vec<String> = if urls.push.is_empty() {
            urls.fetch.clone()
        } else {
            urls.push.clone()
        };
        for pu in push_urls {
            println!("{name}\t{pu} (push)");
        }
    }
    Ok(())
}

fn cmd_remove(rest: &[String]) -> Result<()> {
    if rest.len() != 1 {
        return Err(usage_exit(&["git remote remove <name>"]));
    }
    let name = &rest[0];
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    if !remote_section_exists(&config, name) {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: format!("error: No such remote: '{name}'"),
        }));
    }

    let mut to_delete: Vec<String> = Vec::new();
    let mut skipped_branch_names: BTreeSet<String> = BTreeSet::new();
    let remote = build_remote_stub(&config, name);
    let all_names = collect_remote_names_from_config(&config);
    let others: Vec<String> = all_names.into_iter().filter(|n| n != name).collect();

    let all_refs = refs::list_refs(&git_dir, "refs/")?;
    for (refname, _) in &all_refs {
        let mut mapped_src: Option<String> = None;
        if remote_find_tracking_src(&remote, refname, &mut mapped_src).is_ok()
            && mapped_src.is_some()
        {
            let mut keep = false;
            for o in &others {
                let mut s2: Option<String> = None;
                let r2 = build_remote_stub(&config, o);
                if remote_find_tracking_src(&r2, refname, &mut s2).is_ok() && s2.is_some() {
                    keep = true;
                    break;
                }
            }
            if !keep {
                if refname.starts_with("refs/remotes/") {
                    to_delete.push(refname.clone());
                } else if let Some(short) = refname.strip_prefix("refs/heads/") {
                    skipped_branch_names.insert(short.to_owned());
                }
            }
        }
    }

    let config_path = git_dir.join("config");
    let mut config_file = load_or_create_config_file(&config_path)?;
    unset_branch_remote_for(&mut config_file, name)?;
    // Git's `rm` calls `handle_push_default(name, NULL)`: a *local* `remote.pushDefault` that names
    // the removed remote is unset (global/system pushDefault is left untouched).
    unset_local_push_default_if(&mut config_file, name)?;
    let section = format!("remote.{name}");
    if !config_file.remove_section(&section)? {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: format!("error: No such remote: '{name}'"),
        }));
    }
    config_file.write().context("writing config")?;

    for r in &to_delete {
        let _ = refs::delete_ref(&git_dir, r);
    }

    let packed_refs_path = git_dir.join("packed-refs");
    if packed_refs_path.is_file() {
        let prefix = format!("refs/remotes/{name}/");
        let content = std::fs::read_to_string(&packed_refs_path).context("reading packed-refs")?;
        let filtered: String = content
            .lines()
            .filter(|line| {
                if line.starts_with('#') || line.starts_with('^') {
                    return true;
                }
                if let Some(refname) = line.split_whitespace().nth(1) {
                    !refname.starts_with(&prefix)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let filtered = if filtered.is_empty() || filtered.ends_with('\n') {
            filtered
        } else {
            format!("{filtered}\n")
        };
        std::fs::write(&packed_refs_path, filtered).context("writing packed-refs")?;
    }

    if !skipped_branch_names.is_empty() {
        if let (1, Some(b)) = (
            skipped_branch_names.len(),
            skipped_branch_names.iter().next(),
        ) {
            eprintln!(
                "Note: A branch outside the refs/remotes/ hierarchy was not removed;\n\
                 to delete it, use:\n  git branch -d {b}"
            );
        } else {
            eprintln!(
                "Note: Some branches outside the refs/remotes/ hierarchy were not removed;\n\
                 to delete them, use:"
            );
            for b in &skipped_branch_names {
                eprintln!("  git branch -d {b}");
            }
        }
    }

    Ok(())
}

fn unset_branch_remote_for(config_file: &mut ConfigFile, remote: &str) -> Result<()> {
    let keys: Vec<String> = config_file
        .entries
        .iter()
        .filter_map(|e| {
            let p: Vec<&str> = e.key.splitn(3, '.').collect();
            if p.len() == 3 && p[0] == "branch" && (p[2] == "remote" || p[2] == "merge") {
                Some(format!("{}.{}", p[0], p[1]))
            } else {
                None
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    for branch_section in keys {
        let rkey = format!("{branch_section}.remote");
        if config_file
            .entries
            .iter()
            .rev()
            .find(|e| e.key == rkey)
            .and_then(|e| e.value.as_deref())
            == Some(remote)
        {
            let _ = config_file.unset(&format!("{branch_section}.remote"));
            let _ = config_file.unset(&format!("{branch_section}.merge"));
        }
        let prkey = format!("{branch_section}.pushremote");
        if config_file
            .entries
            .iter()
            .rev()
            .find(|e| e.key == prkey)
            .and_then(|e| e.value.as_deref())
            == Some(remote)
        {
            let _ = config_file.unset(&prkey);
        }
    }
    Ok(())
}

struct RemoteStub {
    fetch: Vec<FetchRefspec>,
}

fn build_remote_stub(config: &ConfigSet, name: &str) -> RemoteStub {
    // Git's `remote->fetch` is populated purely from `remote.<name>.fetch` config lines; the
    // implicit default refspec is only applied by `git fetch`, not by `remote show`/`remote rm`.
    RemoteStub {
        fetch: crate::commands::fetch::collect_refspecs(config, &format!("remote.{name}.fetch")),
    }
}

fn remote_find_tracking_src(
    remote: &RemoteStub,
    dst: &str,
    src_out: &mut Option<String>,
) -> Result<()> {
    *src_out = None;
    let mut refspec = FetchRefspec {
        src: String::new(),
        dst: dst.to_owned(),
        force: false,
        negative: false,
    };
    for rs in &remote.fetch {
        if rs.negative {
            continue;
        }
        refspec.dst = dst.to_owned();
        if let Some(src) = reverse_map_src(&rs.dst, &rs.src, dst) {
            refspec.src = src;
            *src_out = Some(refspec.src.clone());
            return Ok(());
        }
    }
    Err(anyhow::anyhow!("not tracked"))
}

fn reverse_map_src(dst_pat: &str, src_pat: &str, local_ref: &str) -> Option<String> {
    if let Some(star_pos) = dst_pat.find('*') {
        let prefix = &dst_pat[..star_pos];
        let suffix = &dst_pat[star_pos + 1..];
        if local_ref.starts_with(prefix) && local_ref.ends_with(suffix) {
            let matched = &local_ref[prefix.len()..local_ref.len() - suffix.len()];
            return Some(src_pat.replacen('*', matched, 1));
        }
    } else if dst_pat == local_ref {
        return Some(src_pat.to_owned());
    }
    None
}

/// Migrate a legacy `$GIT_DIR/remotes/<name>` or `$GIT_DIR/branches/<name>` remote into config
/// (Git's `migrate_file`): write `remote.<name>.{url,push,fetch}` then unlink the source file.
fn migrate_remote_file(git_dir: &Path, name: &str) -> Result<()> {
    let remotes_file = git_dir.join("remotes").join(name);
    let branches_file = git_dir.join("branches").join(name);

    let (url, push, fetch): (Vec<String>, Vec<String>, Vec<String>) = if remotes_file.is_file() {
        // `$GIT_DIR/remotes/<name>`: `URL:`, `Push:`, `Pull:` lines.
        let content = std::fs::read_to_string(&remotes_file).context("reading remotes file")?;
        let mut url = Vec::new();
        let mut push = Vec::new();
        let mut fetch = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("URL:") {
                url.push(rest.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("Push:") {
                push.push(rest.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("Pull:") {
                fetch.push(rest.trim().to_owned());
            }
        }
        (url, push, fetch)
    } else {
        // `$GIT_DIR/branches/<name>`: a single `url` or `url#branch` line.
        let raw = std::fs::read_to_string(&branches_file).context("reading branches file")?;
        let line = raw.lines().next().unwrap_or("").trim().to_owned();
        let (url_part, frag) = match line.split_once('#') {
            Some((u, b)) => (u.trim().to_owned(), b.trim().to_owned()),
            None => {
                let cfg = load_local_config(git_dir)?;
                (
                    line.clone(),
                    crate::commands::fetch::repo_default_branch_name(&cfg),
                )
            }
        };
        let fetch = vec![format!("refs/heads/{frag}:refs/heads/{name}")];
        let push = vec![format!("HEAD:refs/heads/{frag}")];
        (vec![url_part], push, fetch)
    };

    let config_path = git_dir.join("config");
    let mut config_file = load_or_create_config_file(&config_path)?;
    for u in &url {
        config_file.add_value(&format!("remote.{name}.url"), u)?;
    }
    for p in &push {
        config_file.add_value(&format!("remote.{name}.push"), p)?;
    }
    for f in &fetch {
        config_file.add_value(&format!("remote.{name}.fetch"), f)?;
    }
    config_file.write().context("writing config")?;

    if remotes_file.is_file() {
        let _ = std::fs::remove_file(&remotes_file);
    } else {
        let _ = std::fs::remove_file(&branches_file);
    }
    Ok(())
}

fn cmd_rename(rest: &[String], _from_add: bool) -> Result<()> {
    let mut i = 0usize;
    let mut show_progress = false;
    while i < rest.len() && (rest[i] == "--progress" || rest[i] == "--no-progress") {
        show_progress = rest[i] == "--progress";
        i += 1;
    }
    let rest = &rest[i..];
    if rest.len() != 2 {
        return Err(usage_exit(&[
            "git remote rename [--[no-]progress] <old> <new>",
        ]));
    }
    let old = rest[0].clone();
    let new = rest[1].clone();
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;

    // A remote may be configured via `remote.<name>.*` config OR a legacy `$GIT_DIR/remotes/<name>`
    // / `$GIT_DIR/branches/<name>` file. Renaming a file-based remote to its own name migrates it
    // into config (Git's `migrate_file`).
    let in_config = remote_section_exists(&config, &old);
    let remotes_file = git_dir.join("remotes").join(&old);
    let branches_file = git_dir.join("branches").join(&old);
    let from_file = remotes_file.is_file() || branches_file.is_file();
    if !in_config && !from_file {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: format!("error: No such remote: '{old}'"),
        }));
    }
    if old == new && !in_config && from_file {
        return migrate_remote_file(&git_dir, &old);
    }
    // Git's collision check uses `remote_is_configured(..., in_repo=1)`: a remote that exists only
    // via global/system config (e.g. a stray `remote.<new>.prune`) does not block the rename.
    if remote_configured_in_repo(&git_dir, &new)? {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 3,
            message: format!("error: remote {new} already exists."),
        }));
    }
    if !valid_remote_name(&new) {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: format!("fatal: '{new}' is not a valid remote name"),
        }));
    }
    // Git's `mv` does NOT run the subset/superset collision check (that is only for `add`), so
    // nesting a remote into itself (`parent` -> `parent/child`) is permitted.

    let old_prefix = format!("refs/remotes/{old}/");
    let new_prefix = format!("refs/remotes/{new}/");

    // Capture every loose tracking ref under the old remote (with its symref target, if any) before
    // any mutation, so the move can be performed ref-by-ref. This handles nesting a remote into
    // itself (`parent` -> `parent/child`) where a plain directory rename would be impossible, and
    // captures broken/unborn symrefs (e.g. HEAD -> a nonexistent branch) that `list_refs` skips.
    let old_remote_dir = git_dir.join("refs/remotes").join(&old);
    let mut refs_to_move = collect_loose_remote_refs(&old_remote_dir, &git_dir);
    // Clone now records tracking refs in `packed-refs`, so the rename must also move any packed
    // `refs/remotes/<old>/...` entry that has no loose counterpart (t5505 'rename a remote').
    let already: std::collections::HashSet<String> =
        refs_to_move.iter().map(|(r, _, _)| r.clone()).collect();
    for (refname, oid) in collect_packed_remote_refs(&git_dir, &old_prefix) {
        if !already.contains(&refname) {
            refs_to_move.push((refname, Some(oid), None));
        }
    }
    refs_to_move.sort_by(|a, b| a.0.cmp(&b.0));

    // Detect a ref-namespace conflict (Git's transaction prepare failing with NAME_CONFLICT): a new
    // ref name that collides with an existing ref as a prefix/suffix. Renaming into itself only
    // conflicts when a sibling ref already occupies the nested path. Git renames the config section
    // *before* this transaction, so on conflict the config rename is left in place (the failed
    // `git remote rename` still leaves `remote.<new>` configured); compute the flag now but defer
    // the error until after the config is written.
    let has_ref_conflict = refs_to_move.iter().any(|(old_ref, _, _)| {
        let Some(tail) = old_ref.strip_prefix(&old_prefix) else {
            return false;
        };
        let new_ref = format!("{new_prefix}{tail}");
        new_ref != *old_ref
            && ref_name_conflicts(&git_dir, &new_ref, &refs_to_move, &old_prefix, &new_prefix)
    });

    let config_path = git_dir.join("config");
    let mut config_file = load_or_create_config_file(&config_path)?;
    let old_section = format!("remote.{old}");
    let new_section = format!("remote.{new}");
    if !config_file.rename_section(&old_section, &new_section)? {
        bail!("No such remote: '{old}'");
    }

    // Git renames the config *section* first, then prepares the ref-rename transaction. On a
    // NAME_CONFLICT it `die(NULL)`s *before* rewriting the fetch refspec or `branch.*` /
    // pushDefault config (builtin/remote.c). So on conflict only the section rename persists: the
    // fetch refspec keeps pointing at the OLD `refs/remotes/<old>/` namespace, which is exactly
    // what lets a follow-up `git remote remove <new>` clean up the stray ref that caused the
    // conflict (t5505 "rename can unnest a remote" depends on this cleanup of test 128's leftover).
    if has_ref_conflict {
        config_file.write().context("writing config")?;
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "error: renaming remote references failed: The remote you are trying to rename has conflicting references in the\n\
                 new target refspec. This is most likely caused by you trying to nest\n\
                 one remote in another, which is not supported.".to_owned(),
        }));
    }

    // Git only rewrites refspecs (and renames refs) when at least one fetch refspec maps into the
    // old remote's tracking namespace (`:refs/remotes/<old>/`). Each such refspec has `<old>`
    // replaced with `<new>` just after `:refs/remotes/`; others are left alone with a warning.
    let old_remote_context = format!(":refs/remotes/{old}/");
    let fetch_key = format!("remote.{new}.fetch");
    let current_fetch: Vec<String> = config_file
        .entries
        .iter()
        .filter(|e| e.key == fetch_key)
        .filter_map(|e| e.value.clone())
        .collect();
    let refspecs_need_update = current_fetch
        .iter()
        .any(|v| v.contains(&old_remote_context));

    if !current_fetch.is_empty() {
        config_file.unset(&fetch_key)?;
        for raw in &current_fetch {
            let new_val = if let Some(pos) = raw.find(&old_remote_context) {
                let head = &raw[..pos + ":refs/remotes/".len()];
                let tail = &raw[pos + old_remote_context.len()..];
                format!("{head}{new}/{tail}")
            } else {
                eprintln!(
                    "warning: Not updating non-default fetch refspec\n\t{raw}\n\tPlease update the configuration manually if necessary."
                );
                raw.clone()
            };
            config_file.add_value(&fetch_key, &new_val)?;
        }
    }

    rename_branch_config_remote(&mut config_file, &old, &new)?;
    update_push_default_if_local(&mut config_file, &old, &new)?;

    config_file.write().context("writing config")?;

    // Only rewrite refs when at least one fetch refspec maps into the old tracking namespace.
    if !refspecs_need_update {
        return Ok(());
    }

    // Emit the trace2 progress region Git surrounds the ref renames with (t5505 `test_region`).
    if show_progress {
        if let Ok(trace) = std::env::var("GIT_TRACE2_EVENT") {
            let _ = crate::trace2_region_json(&trace, "progress", "Renaming remote references");
        }
    }

    let identity = git_identity_line()?;
    // Capture each ref's existing reflog before deletion so it can be carried over to the new ref
    // (Git renames the reflog along with the ref).
    let saved_reflogs: HashMap<String, String> = refs_to_move
        .iter()
        .filter_map(|(old_ref, _, _)| {
            let p = git_dir.join("logs").join(old_ref);
            std::fs::read_to_string(&p)
                .ok()
                .map(|c| (old_ref.clone(), c))
        })
        .collect();
    // Delete every old ref first so nesting `parent` into `parent/child` does not leave a file
    // blocking the new directory, then recreate at the renamed path.
    for (old_ref, _, _) in &refs_to_move {
        let _ = refs::delete_ref(&git_dir, old_ref);
    }
    for (old_ref, oid, sym) in &refs_to_move {
        let Some(tail) = old_ref.strip_prefix(&old_prefix) else {
            continue;
        };
        let new_ref = format!("{new_prefix}{tail}");
        if let Some(target) = sym {
            // Re-point the symref's target into the new namespace when it referenced the old one.
            let new_target = target
                .strip_prefix(&old_prefix)
                .map(|t| format!("{new_prefix}{t}"))
                .unwrap_or_else(|| target.clone());
            refs::write_symbolic_ref(&git_dir, &new_ref, &new_target)?;
            continue;
        }
        let Some(oid) = oid else {
            continue;
        };
        refs::write_ref(&git_dir, &new_ref, oid)?;
        // Restore the carried-over reflog, then append the rename entry (current OID for both
        // old and new value), matching Git's reflog rename.
        restore_reflog(
            &git_dir,
            &new_ref,
            saved_reflogs.get(old_ref).map(String::as_str),
        );
        let msg = format!("remote: renamed {old_ref} to {new_ref}");
        refs::append_reflog(&git_dir, &new_ref, oid, oid, &identity, &msg, true)?;
    }

    Ok(())
}

/// Restore a previously captured reflog (`old_contents`) at the `new_ref`'s loose log path. When
/// no reflog was carried over, any reflog stub created by `write_ref` is removed so only the
/// subsequent rename entry remains.
fn restore_reflog(git_dir: &Path, new_ref: &str, old_contents: Option<&str>) {
    let new_log = git_dir.join("logs").join(new_ref);
    if let Some(parent) = new_log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match old_contents {
        Some(contents) => {
            let _ = std::fs::write(&new_log, contents);
        }
        None => {
            let _ = std::fs::remove_file(&new_log);
        }
    }
}

/// Whether creating `new_ref` would clash with an existing ref in the store as a name-prefix or
/// name-suffix (Git's `REF_TRANSACTION_ERROR_NAME_CONFLICT`). Refs in `refs_to_move` are excluded
/// because they are being deleted as part of the same rename; their *renamed* destinations are the
/// only ones that could legitimately clash.
fn ref_name_conflicts(
    git_dir: &Path,
    new_ref: &str,
    refs_to_move: &[(String, Option<ObjectId>, Option<String>)],
    old_prefix: &str,
    new_prefix: &str,
) -> bool {
    // Destinations of all moved refs.
    let dests: Vec<String> = refs_to_move
        .iter()
        .filter_map(|(r, _, _)| {
            r.strip_prefix(old_prefix)
                .map(|t| format!("{new_prefix}{t}"))
        })
        .collect();
    let conflicts_with = |other: &str| -> bool {
        other != new_ref
            && (other.starts_with(&format!("{new_ref}/"))
                || new_ref.starts_with(&format!("{other}/")))
    };
    // A moved ref's old name (which occupies its directory path until deleted) being a strict
    // prefix of the destination is a D/F conflict Git rejects even within one transaction
    // (e.g. delete `parent/child`, create `parent/child/child`).
    if refs_to_move
        .iter()
        .any(|(old_name, _, _)| new_ref.starts_with(&format!("{old_name}/")))
    {
        return true;
    }
    // A moved ref's destination clashing with another destination.
    if dests.iter().any(|o| conflicts_with(o)) {
        return true;
    }
    // An existing ref (not being moved away) clashing with the destination.
    let Ok(all) = refs::list_refs(git_dir, "refs/remotes/") else {
        return false;
    };
    all.iter()
        .map(|(r, _)| r.as_str())
        .filter(|r| !refs_to_move.iter().any(|(m, _, _)| m == r))
        .any(conflicts_with)
}

/// Walk a `refs/remotes/<old>/` directory and return every loose ref as `(full_name, oid, symref)`.
///
/// Symbolic refs yield `(name, None, Some(target))`; direct refs yield `(name, Some(oid), None)`.
/// Broken/unborn symrefs (whose target does not exist) are still captured, unlike `list_refs`.
fn collect_loose_remote_refs(
    dir: &Path,
    git_dir: &Path,
) -> Vec<(String, Option<ObjectId>, Option<String>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(git_dir) else {
                continue;
            };
            let refname = rel.to_string_lossy().replace('\\', "/");
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let content = content.trim();
            if let Some(target) = content.strip_prefix("ref: ") {
                out.push((refname, None, Some(target.trim().to_owned())));
            } else if let Ok(oid) = ObjectId::from_hex(content) {
                out.push((refname, Some(oid), None));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Read every `packed-refs` entry whose name begins with `old_prefix` (e.g.
/// `refs/remotes/origin/`). Returns `(refname, oid)` pairs; peeled (`^...`) lines are skipped since
/// they only annotate the preceding ref. Used so `git remote rename` moves packed tracking refs
/// (clone now writes tracking refs to `packed-refs`).
fn collect_packed_remote_refs(git_dir: &Path, old_prefix: &str) -> Vec<(String, ObjectId)> {
    let mut out = Vec::new();
    let Ok(content) = std::fs::read_to_string(git_dir.join("packed-refs")) else {
        return out;
    };
    for line in content.lines() {
        let line = line.trim_end();
        if line.starts_with('#') || line.starts_with('^') || line.is_empty() {
            continue;
        }
        let Some((oid_str, refname)) = line.split_once(' ') else {
            continue;
        };
        if !refname.starts_with(old_prefix) {
            continue;
        }
        if let Ok(oid) = ObjectId::from_hex(oid_str.trim()) {
            out.push((refname.to_owned(), oid));
        }
    }
    out
}

fn git_identity_line() -> Result<String> {
    let name = std::env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| "User".to_owned());
    let email =
        std::env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| "user@example.com".to_owned());
    Ok(format!("{name} <{email}> 1112912173 -0700"))
}

fn rename_branch_config_remote(config_file: &mut ConfigFile, old: &str, new: &str) -> Result<()> {
    let branches: BTreeSet<String> = config_file
        .entries
        .iter()
        .filter_map(|e| {
            let p: Vec<&str> = e.key.splitn(3, '.').collect();
            if p.len() == 3 && p[0] == "branch" {
                Some(p[1].to_owned())
            } else {
                None
            }
        })
        .collect();

    for b in branches {
        let rkey = format!("branch.{b}.remote");
        if config_file
            .entries
            .iter()
            .rev()
            .find(|e| e.key == rkey)
            .and_then(|e| e.value.as_deref())
            == Some(old)
        {
            config_file.set(&rkey, new)?;
        }
        let pkey = format!("branch.{b}.pushremote");
        if config_file
            .entries
            .iter()
            .rev()
            .find(|e| e.key == pkey)
            .and_then(|e| e.value.as_deref())
            == Some(old)
        {
            // Write the canonical camelCase variable name Git uses (t5505 greps for `pushRemote`).
            config_file.set(&format!("branch.{b}.pushRemote"), new)?;
        }
    }
    Ok(())
}

fn update_push_default_if_local(config_file: &mut ConfigFile, old: &str, new: &str) -> Result<()> {
    let key = "remote.pushdefault";
    if config_file
        .entries
        .iter()
        .rev()
        .find(|e| e.key == key)
        .and_then(|e| e.value.as_deref())
        == Some(old)
    {
        // Preserve Git's camelCase variable name `pushDefault` (t5505 greps for it).
        config_file.set("remote.pushDefault", new)?;
    }
    Ok(())
}

/// Unset the repository-local `remote.pushDefault` when it names `remote` (Git's
/// `handle_push_default(remote, NULL)` for `git remote remove`).
fn unset_local_push_default_if(config_file: &mut ConfigFile, remote: &str) -> Result<()> {
    let key = "remote.pushdefault";
    let matches = config_file
        .entries
        .iter()
        .rev()
        .find(|e| e.key == key)
        .and_then(|e| e.value.as_deref())
        == Some(remote);
    if matches {
        let _ = config_file.unset(key);
    }
    Ok(())
}

fn cmd_get_url(rest: &[String]) -> Result<()> {
    let mut push = false;
    let mut all = false;
    let mut i = 0usize;
    while i < rest.len() {
        match rest[i].as_str() {
            "--push" => {
                push = true;
                i += 1;
            }
            "--all" => {
                all = true;
                i += 1;
            }
            _ if rest[i].starts_with('-') => bail!("unknown option: {}", rest[i]),
            _ => break,
        }
    }
    let rest = &rest[i..];
    if rest.len() != 1 {
        return Err(usage_exit(&["git remote get-url [--push] [--all] <name>"]));
    }
    let name = &rest[0];
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    if !remote_section_exists(&config, name) {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: format!("error: No such remote '{name}'"),
        }));
    }
    let Some(urls) = remote_urls_effective(&config, name) else {
        bail!("No URL configured for remote '{name}'");
    };
    let list: Vec<String> = if push {
        if urls.push.is_empty() {
            urls.fetch.clone()
        } else {
            urls.push.clone()
        }
    } else {
        urls.fetch.clone()
    };
    if all {
        for u in list {
            println!("{u}");
        }
    } else if let Some(f) = list.first() {
        println!("{f}");
    }
    Ok(())
}

fn cmd_set_url(rest: &[String]) -> Result<()> {
    let mut push = false;
    let mut add = false;
    let mut delete = false;
    let mut i = 0usize;
    while i < rest.len() {
        match rest[i].as_str() {
            "--push" => {
                push = true;
                i += 1;
            }
            "--add" => {
                add = true;
                i += 1;
            }
            "--delete" => {
                delete = true;
                i += 1;
            }
            _ if rest[i].starts_with('-') => bail!("unknown option: {}", rest[i]),
            _ => break,
        }
    }
    if add && delete {
        bail!("--add --delete doesn't make sense");
    }
    let rest = &rest[i..];
    let set_url_usage = || {
        usage_exit(&[
            "git remote set-url [--push] <name> <newurl> [<oldurl>]",
            "git remote set-url --add <name> <newurl>",
            "git remote set-url --delete <name> <url>",
        ])
    };
    if rest.is_empty() || rest.len() > 3 {
        return Err(set_url_usage());
    }
    let name = &rest[0];
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    if !remote_section_exists(&config, name) {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: format!("error: No such remote '{name}'"),
        }));
    }
    let config_path = git_dir.join("config");
    let mut config_file = load_or_create_config_file(&config_path)?;
    let key = if push {
        format!("remote.{name}.pushurl")
    } else {
        format!("remote.{name}.url")
    };

    if (!delete && rest.len() == 2) || add {
        let newurl = rest
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("usage: git remote set-url --add <name> <newurl>"))?;
        if add {
            config_file.add_value(&key, newurl)?;
        } else {
            config_file.set(&key, newurl)?;
        }
        write_config_respecting_lock(&config_file, &config_path)?;
        return Ok(());
    }

    if delete {
        let pat = rest
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("usage: git remote set-url --delete <name> <url>"))?;
        let re = regex::Regex::new(pat)
            .map_err(|e| anyhow::anyhow!("Invalid old URL pattern: {pat}: {e}"))?;
        let vals: Vec<String> = config_file
            .entries
            .iter()
            .filter(|e| e.key == key)
            .filter_map(|e| e.value.clone())
            .collect();
        let matches_ct = vals.iter().filter(|v| re.is_match(v)).count();
        if matches_ct == 0 {
            bail!("No such URL found: {pat}");
        }
        if !push && matches_ct == vals.len() {
            bail!("Will not delete all non-push URLs");
        }
        // Remove the matching value lines entirely (Git deletes via multivar unset, not by
        // blanking the value). Keep the section header even if it becomes empty.
        config_file
            .unset_matching(&key, Some(pat), true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        write_config_respecting_lock(&config_file, &config_path)?;
        return Ok(());
    }

    let newurl = rest
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("missing new URL"))?;
    let oldpat = rest
        .get(2)
        .ok_or_else(|| anyhow::anyhow!("missing old URL pattern"))?;
    let re = regex::Regex::new(oldpat)
        .map_err(|e| anyhow::anyhow!("Invalid old URL pattern: {oldpat}: {e}"))?;
    let vals = config_file
        .entries
        .iter()
        .filter(|e| e.key == key)
        .filter_map(|e| e.value.as_deref())
        .collect::<Vec<_>>();
    if !vals.iter().any(|v| re.is_match(v)) {
        bail!("No such URL found: {oldpat}");
    }
    config_file.replace_all(&key, newurl, Some(oldpat))?;
    write_config_respecting_lock(&config_file, &config_path)?;
    Ok(())
}

fn cmd_set_branches(rest: &[String]) -> Result<()> {
    // Git uses `parse_options` (permuting), so `--add` may appear anywhere among the positional
    // arguments, e.g. `set-branches scratch --add other`. Collect it wherever it lands; `--` ends
    // option parsing.
    let mut add_mode = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut saw_dashdash = false;
    for a in rest {
        if !saw_dashdash && a == "--" {
            saw_dashdash = true;
        } else if !saw_dashdash && a == "--add" {
            add_mode = true;
        } else {
            positionals.push(a.clone());
        }
    }
    if positionals.is_empty() {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 129,
            message: "error: no remote specified".to_owned(),
        }));
    }
    let name = positionals[0].clone();
    let branches: Vec<String> = positionals[1..].to_vec();
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    if !remote_section_exists(&config, &name) {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: format!("error: No such remote '{name}'"),
        }));
    }
    let config_path = git_dir.join("config");
    let mut config_file = load_or_create_config_file(&config_path)?;
    let fetch_key = format!("remote.{name}.fetch");
    let (fetch_mirror, push_mirror) = mirror_flags(&config, &name);
    if !add_mode {
        config_file.unset(&fetch_key)?;
    }
    for branch in &branches {
        let pat = if fetch_mirror {
            if branch.starts_with("refs/") {
                format!("+{branch}:{branch}")
            } else if branch.starts_with("heads/") {
                format!("+refs/{branch}:refs/{branch}")
            } else {
                format!("+refs/heads/{branch}:refs/heads/{branch}")
            }
        } else {
            format!("+refs/heads/{branch}:refs/remotes/{name}/{branch}")
        };
        config_file.add_value(&fetch_key, &pat)?;
    }
    if push_mirror && !branches.is_empty() {
        bail!("push mirrors do not allow you to specify refs");
    }
    config_file.write().context("writing config")?;
    Ok(())
}

fn cmd_set_head(rest: &[String]) -> Result<()> {
    let mut auto = false;
    let mut delete = false;
    let mut i = 0usize;
    while i < rest.len() {
        match rest[i].as_str() {
            "-a" | "--auto" => {
                auto = true;
                i += 1;
            }
            "-d" | "--delete" => {
                delete = true;
                i += 1;
            }
            _ if rest[i].starts_with('-') => bail!("unknown option: {}", rest[i]),
            _ => break,
        }
    }
    let rest = &rest[i..];
    if rest.is_empty() {
        return Err(usage_exit(&[
            "git remote set-head <name> (-a | --auto | -d | --delete | <branch>)",
        ]));
    }
    let remote_name = rest[0].clone();
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    if !remote_section_exists(&config, &remote_name) {
        bail!("No such remote '{}'", remote_name);
    }
    let head_ref = format!("refs/remotes/{remote_name}/HEAD");

    if delete {
        if rest.len() != 1 {
            bail!("usage: git remote set-head --delete <name>");
        }
        refs::delete_ref(&git_dir, &head_ref)
            .map_err(|_| anyhow::Error::msg(format!("error: Could not delete {}", head_ref)))?;
        return Ok(());
    }

    if auto {
        if rest.len() != 1 {
            bail!("usage: git remote set-head --auto <name>");
        }
        let url = config
            .get(&format!("remote.{remote_name}.url"))
            .unwrap_or_default();
        let heads = if let Some(p) = url_to_local_repo_path(&url) {
            let rgd = find_git_dir(&p)?;
            guess_remote_head_names(&rgd, &config)?
        } else {
            Vec::new()
        };
        if heads.is_empty() {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 1,
                message: "error: Cannot determine remote HEAD".to_owned(),
            }));
        }
        if heads.len() > 1 {
            let mut msg = String::from(
                "error: Multiple remote HEAD branches. Please choose one explicitly with:",
            );
            for h in &heads {
                msg.push_str(&format!("\n  git remote set-head {remote_name} {h}"));
            }
            return Err(anyhow::Error::new(ExplicitExit {
                code: 1,
                message: msg,
            }));
        }
        let head_name = heads[0].clone();
        let target = format!("refs/remotes/{remote_name}/{head_name}");
        let prev = read_remote_head_previous(&git_dir, &remote_name);
        if refs::resolve_ref(&git_dir, &target).is_err() {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 1,
                message: format!("error: Not a valid ref: {target}"),
            }));
        }
        // Match Git's `refs_update_symref_extended`: a pre-existing `<ref>.lock` means the ref is
        // already locked and updating fails with "Could not set up <ref>".
        let head_lock = git_dir.join(format!("{head_ref}.lock"));
        if head_lock.exists() {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 1,
                message: format!("error: Could not set up {head_ref}"),
            }));
        }
        refs::write_symbolic_ref(&git_dir, &head_ref, &target).map_err(|_| {
            anyhow::Error::new(ExplicitExit {
                code: 1,
                message: format!("error: Could not set up {head_ref}"),
            })
        })?;
        report_set_head_auto(&remote_name, &head_name, &prev);
        maybe_downgrade_follow_remote_head(&git_dir, &remote_name)?;
        return Ok(());
    }

    if rest.len() != 2 {
        return Err(usage_exit(&[
            "git remote set-head <name> (-a | --auto | -d | --delete | <branch>)",
        ]));
    }
    let branch = rest[1].trim();
    let short = branch.strip_prefix("refs/heads/").unwrap_or(branch).trim();
    if short.is_empty() {
        bail!("branch name required");
    }
    let target = format!("refs/remotes/{remote_name}/{short}");
    refs::resolve_ref(&git_dir, &target)
        .with_context(|| format!("unknown remote ref '{}'", target))?;
    refs::write_symbolic_ref(&git_dir, &head_ref, &target)
        .with_context(|| format!("could not update {}", head_ref))?;
    println!("{head_ref} -> {target}");
    Ok(())
}

#[derive(Debug, Clone)]
enum RemoteHeadPrevious {
    Missing,
    SymRemoteBranch(String),
    SymOther(String),
    DetachedAt(String),
}

fn read_remote_head_previous(git_dir: &Path, remote: &str) -> RemoteHeadPrevious {
    let head_ref = format!("refs/remotes/{remote}/HEAD");
    if let Ok(Some(target)) = grit_lib::refs::read_symbolic_ref(git_dir, &head_ref) {
        let prefix = format!("refs/remotes/{remote}/");
        if let Some(short) = target.strip_prefix(&prefix) {
            return RemoteHeadPrevious::SymRemoteBranch(short.to_owned());
        }
        return RemoteHeadPrevious::SymOther(target);
    }
    if let Ok(oid) = refs::resolve_ref(git_dir, &head_ref) {
        return RemoteHeadPrevious::DetachedAt(oid.to_hex());
    }
    RemoteHeadPrevious::Missing
}

fn report_set_head_auto(remote: &str, head_name: &str, prev: &RemoteHeadPrevious) {
    let sq = std::env::var("SQ").unwrap_or_else(|_| "'".to_owned());
    match prev {
        RemoteHeadPrevious::Missing => {
            println!("{sq}{remote}/HEAD{sq} is now created and points to {sq}{head_name}{sq}");
        }
        RemoteHeadPrevious::SymRemoteBranch(b) => {
            if b == head_name {
                println!("{sq}{remote}/HEAD{sq} is unchanged and points to {sq}{head_name}{sq}");
            } else {
                println!(
                    "{sq}{remote}/HEAD{sq} has changed from {sq}{b}{sq} and now points to {sq}{head_name}{sq}"
                );
            }
        }
        RemoteHeadPrevious::SymOther(t) => {
            println!(
                "{sq}{remote}/HEAD{sq} used to point to {sq}{t}{sq} (which is not a remote branch), but now points to {sq}{head_name}{sq}"
            );
        }
        RemoteHeadPrevious::DetachedAt(oid) => {
            println!(
                "{sq}{remote}/HEAD{sq} was detached at {sq}{oid}{sq} and now points to {sq}{head_name}{sq}"
            );
        }
    }
}

fn maybe_downgrade_follow_remote_head(git_dir: &Path, remote: &str) -> Result<()> {
    let key = format!("remote.{remote}.followremotehead");
    let config_path = git_dir.join("config");
    let mut config_file = load_or_create_config_file(&config_path)?;
    if config_file
        .entries
        .iter()
        .rev()
        .find(|e| e.key == key)
        .and_then(|e| e.value.as_deref())
        .map(|v| v.eq_ignore_ascii_case("always"))
        == Some(true)
    {
        config_file.set(&key, "warn")?;
        config_file.write().context("writing config")?;
    }
    Ok(())
}

/// Determine the candidate HEAD branch names for `set-head --auto`, mirroring Git's
/// `guess_remote_head` with `REMOTE_GUESS_HEAD_ALL` (via `get_head_names`).
///
/// If the remote advertises HEAD as a symbolic ref, its target branch is used directly.
/// Otherwise every `refs/heads/*` whose OID equals HEAD's OID is returned (the caller errors
/// when more than one matches).
///
/// # Parameters
/// - `remote_git` — git directory of the remote repository.
/// - `config` — local config, used to resolve the repository default branch name.
fn guess_remote_head_names(remote_git: &Path, config: &ConfigSet) -> Result<Vec<String>> {
    let odb = Odb::new(&remote_git.join("objects"));
    let entries = ls_remote(
        remote_git,
        &odb,
        &LsRemoteOpts {
            heads: false,
            tags: false,
            refs_only: false,
            symref: true,
            all_symrefs: false,
            patterns: Vec::new(),
        },
    )?;
    let head = entries.iter().find(|e| e.name == "HEAD");
    let Some(head) = head else {
        return Ok(Vec::new());
    };
    // Fast path: the transport told us exactly where HEAD points.
    if let Some(target) = &head.symref_target {
        if let Some(b) = target.strip_prefix("refs/heads/") {
            return Ok(vec![b.to_owned()]);
        }
    }
    let head_oid = head.oid;
    let _ = config;
    // REMOTE_GUESS_HEAD_ALL: collect every head pointing at the same OID as HEAD.
    let mut out: Vec<String> = entries
        .iter()
        .filter_map(|e| {
            let b = e.name.strip_prefix("refs/heads/")?;
            (e.oid == head_oid).then(|| b.to_owned())
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

fn url_to_local_repo_path(url: &str) -> Option<PathBuf> {
    let p = if let Some(s) = url.strip_prefix("file://") {
        PathBuf::from(s)
    } else {
        PathBuf::from(url)
    };
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

fn cmd_show(rest: &[String], global_verbose: bool) -> Result<()> {
    let mut no_query = false;
    let mut i = 0usize;
    while i < rest.len() {
        if rest[i] == "-n" {
            no_query = true;
            i += 1;
        } else if rest[i] == "-v" || rest[i] == "--verbose" {
            i += 1;
        } else if rest[i].starts_with('-') {
            bail!("unknown option: {}", rest[i]);
        } else {
            break;
        }
    }
    let names: Vec<String> = rest[i..].to_vec();
    if names.is_empty() {
        // Git's `show` with no remote name falls back to `show_all()` (== `git remote`).
        return cmd_list(global_verbose);
    }
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    let repo = Repository::open(&git_dir, None).map_err(|e| anyhow::anyhow!("{}", e))?;
    for name in names {
        show_one_remote(&repo, &config, &git_dir, &name, no_query)?;
    }
    Ok(())
}

fn show_one_remote(
    repo: &Repository,
    config: &ConfigSet,
    git_dir: &Path,
    name: &str,
    no_query: bool,
) -> Result<()> {
    if !remote_section_exists(config, name) {
        bail!("No such remote '{}'", name);
    }
    let Some(urls) = remote_urls_effective(config, name) else {
        bail!("No URL configured for remote '{}'", name);
    };
    let fetch0 = urls.fetch.first().cloned().unwrap_or_default();
    println!("* remote {name}");
    println!("  Fetch URL: {fetch0}");
    let push_list: Vec<String> = if urls.push.is_empty() {
        urls.fetch.clone()
    } else {
        urls.push.clone()
    };
    if push_list.is_empty() {
        println!("  Push  URL: (no URL)");
    } else {
        for pu in push_list {
            println!("  Push  URL: {pu}");
        }
    }

    let remote_stub = build_remote_stub(config, name);
    let (_fetch_mirror, push_mirror) = mirror_flags(config, name);

    let mut remote_git_dir: Option<PathBuf> = None;
    let mut advertised: HashMap<String, ObjectId> = HashMap::new();
    if !no_query {
        if let Some(p) = url_to_local_repo_path(&fetch0) {
            if let Ok(rgd) = find_git_dir(&p) {
                let odb = Odb::new(&rgd.join("objects"));
                if let Ok(entries) = ls_remote(&rgd, &odb, &LsRemoteOpts::default()) {
                    remote_git_dir = Some(rgd);
                    for e in entries {
                        if e.name == "HEAD" || e.name.starts_with("refs/tags/") {
                            continue;
                        }
                        if e.name.starts_with("refs/heads/") {
                            advertised.insert(e.name.clone(), e.oid);
                        }
                    }
                }
            }
        }
    }

    if no_query {
        println!("  HEAD branch: (not queried)");
    } else if advertised.is_empty() {
        println!("  HEAD branch: (unknown)");
    } else if let Some(ref rgd) = remote_git_dir {
        if let Ok(Some(sym_target)) = grit_lib::refs::read_symbolic_ref(rgd, "HEAD") {
            if let Some(b) = sym_target.strip_prefix("refs/heads/") {
                println!("  HEAD branch: {b}");
            } else {
                println!("  HEAD branch: (unknown)");
            }
        } else {
            let mut head_candidates: Vec<String> = advertised
                .keys()
                .filter_map(|r| r.strip_prefix("refs/heads/").map(|s| s.to_owned()))
                .collect();
            head_candidates.sort();
            head_candidates.dedup();
            if head_candidates.len() == 1 {
                println!("  HEAD branch: {}", head_candidates[0]);
            } else if head_candidates.is_empty() {
                println!("  HEAD branch: (unknown)");
            } else {
                println!("  HEAD branch (remote HEAD is ambiguous, may be one of the following):");
                for h in head_candidates {
                    println!("    {h}");
                }
            }
        }
    } else {
        println!("  HEAD branch: (unknown)");
    }

    let mut listed: Vec<(String, String)> = Vec::new();
    if !no_query {
        // Build the fetch map exactly like Git's `get_ref_states`: only advertised refs that match
        // a positive fetch refspec source participate. Each such ref is classified new/tracked/
        // skipped; stale local tracking refs (no longer present on the remote) are appended.
        let mut mapped_dsts: HashSet<String> = HashSet::new();
        for r in advertised.keys() {
            let Some(branch) = r.strip_prefix("refs/heads/") else {
                continue;
            };
            if ref_excluded_by_fetch_refspecs(r, &remote_stub.fetch) {
                listed.push((branch.to_owned(), "skipped".to_owned()));
                continue;
            }
            let Some(local_ref) = map_ref_through_refspecs(r, &remote_stub.fetch) else {
                continue;
            };
            mapped_dsts.insert(local_ref.clone());
            if refs::resolve_ref(git_dir, &local_ref).is_ok() {
                listed.push((branch.to_owned(), "tracked".to_owned()));
            } else {
                listed.push((
                    branch.to_owned(),
                    format!("new (next fetch will store in remotes/{name})"),
                ));
            }
        }
        // Stale: local tracking refs that are a destination of a fetch refspec but whose source is
        // no longer advertised by the remote.
        let prefix = format!("refs/remotes/{name}/");
        for (lr, _) in refs::list_refs(git_dir, &prefix)? {
            if lr.ends_with("/HEAD") || mapped_dsts.contains(&lr) {
                continue;
            }
            if let Ok(Some(_)) = grit_lib::refs::read_symbolic_ref(git_dir, &lr) {
                continue;
            }
            let branch = lr.strip_prefix(&prefix).unwrap_or(&lr);
            let remote_full = format!("refs/heads/{branch}");
            // Only a destination that the refspec set actually maps to can become stale.
            if map_ref_through_refspecs(&remote_full, &remote_stub.fetch).as_deref() == Some(&lr)
                && !advertised.contains_key(&remote_full)
            {
                listed.push((
                    branch.to_owned(),
                    "stale (use 'git remote prune' to remove)".to_owned(),
                ));
            }
        }
        listed.sort_by(|a, b| a.0.cmp(&b.0));
        merge_remote_branch_status(&mut listed);
    } else {
        // No-query mode (Git's `append_ref_to_tracked_list`): for every non-symbolic local ref,
        // reverse-map it through the remote's fetch refspecs and list the abbreviated remote source
        // name. This surfaces e.g. `refs/heads/main` for a `refs/heads/main:refs/heads/upstream`
        // fetch line, not just `refs/remotes/<name>/*` tracking refs.
        for (lr, _) in refs::list_refs(git_dir, "refs/")? {
            if let Ok(Some(_)) = grit_lib::refs::read_symbolic_ref(git_dir, &lr) {
                continue;
            }
            let mut src: Option<String> = None;
            if remote_find_tracking_src(&remote_stub, &lr, &mut src).is_ok() {
                if let Some(s) = src {
                    listed.push((abbrev_branch(&s).to_owned(), String::new()));
                }
            }
        }
        listed.sort_by(|a, b| a.0.cmp(&b.0));
        listed.dedup();
    }

    if !listed.is_empty() {
        let width = listed.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        let suffix = if no_query {
            " (status not queried)"
        } else {
            ""
        };
        println!("  Remote branches:{suffix}");
        for (b, status) in listed {
            if no_query {
                println!("    {b}");
            } else {
                println!("    {b:width$} {status}", width = width);
            }
        }
    }

    let pull_lines = local_branches_for_pull(config, name);
    if !pull_lines.is_empty() {
        let width = pull_lines
            .iter()
            .map(|(n, _, _)| n.len())
            .max()
            .unwrap_or(0);
        let hdr = if pull_lines.len() == 1 {
            "  Local branch configured for 'git pull':"
        } else {
            "  Local branches configured for 'git pull':"
        };
        println!("{hdr}");
        let any_rebase = pull_lines.iter().any(|(_, reb, _)| *reb);
        for (bn, rebase, merges) in pull_lines {
            print!("    {bn:width$} ", width = width);
            // Merge names are already abbreviated by `local_branches_for_pull`.
            let m0 = merges.first().cloned().unwrap_or_default();
            // Continuation lines are indented by `width + 4`, plus one more when any branch rebases
            // (matching the extra leading space Git prints before "merges with remote").
            let cont_width = if any_rebase { width + 5 } else { width + 4 };
            if rebase {
                println!("rebases onto remote {m0}");
            } else if any_rebase {
                println!(" merges with remote {m0}");
            } else {
                println!("merges with remote {m0}");
            }
            for m in merges.iter().skip(1) {
                println!("{:cont_width$}    and with remote {m}", "");
            }
        }
    }

    if push_mirror {
        println!("  Local refs will be mirrored by 'git push'");
    } else if !no_query {
        let pushes = compute_push_status_lines(repo, config, name, &urls)?;
        if !pushes.is_empty() {
            let width = pushes.iter().map(|(s, _, _)| s.len()).max().unwrap_or(0);
            let width2 = pushes.iter().map(|(_, d, _)| d.len()).max().unwrap_or(0);
            let hdr = if pushes.len() == 1 {
                "  Local ref configured for 'git push':"
            } else {
                "  Local refs configured for 'git push':"
            };
            println!("{hdr}");
            for (src, dest, line) in pushes {
                let formatted = format_push_status_line(width, width2, &src, &dest, line);
                println!("    {formatted}");
            }
        }
    } else {
        // No-query push display (Git's `get_push_ref_states_noquery` + `show_push_info_item`).
        let specs = config.get_all(&format!("remote.{name}.push"));
        // Build (src, dest, forced) triples matching Git's spec interpretation.
        let mut rows: Vec<(String, String, bool)> = Vec::new();
        if specs.is_empty() {
            rows.push(("(matching)".to_owned(), "(matching)".to_owned(), false));
        }
        for s in &specs {
            let (forced, rest) = if let Some(r) = s.strip_prefix('+') {
                (true, r)
            } else {
                (false, s.as_str())
            };
            if rest == ":" {
                rows.push(("(matching)".to_owned(), "(matching)".to_owned(), false));
                continue;
            }
            let (src, dst) = match rest.split_once(':') {
                Some((a, b)) => (a.to_owned(), b.to_owned()),
                // Source-only spec (e.g. `+refs/tags/lastbackup`): dest mirrors the source.
                None => (rest.to_owned(), rest.to_owned()),
            };
            rows.push((src, dst, forced));
        }
        println!("  Local refs configured for 'git push' (status not queried):");
        let width = rows.iter().map(|(s, _, _)| s.len()).max().unwrap_or(0);
        for (src, dst, forced) in rows {
            let verb = if forced { "forces to" } else { "pushes to" };
            println!("    {src:width$} {verb} {dst}");
        }
    }

    Ok(())
}

fn format_push_status_line(
    w1: usize,
    w2: usize,
    src: &str,
    dest: &str,
    status: PushDisplay,
) -> String {
    let pad_src = format!("{src:w1$}");
    let pad_dest = format!("{dest:w2$}");
    match status {
        PushDisplay::Plain => format!("{pad_src} pushes to {dest}"),
        PushDisplay::ForcedPlain => format!("{pad_src} forces to {dest}"),
        PushDisplay::WithStatus(st) => format!("{pad_src} pushes to {pad_dest} ({st})"),
        PushDisplay::ForcedWithStatus(st) => format!("{pad_src} forces to {pad_dest} ({st})"),
    }
}

fn merge_remote_branch_status(listed: &mut Vec<(String, String)>) {
    fn rank(s: &str) -> u8 {
        if s == "tracked" {
            3
        } else if s.contains("stale") {
            2
        } else if s.contains("new (next fetch") {
            1
        } else {
            0
        }
    }
    let mut i = 0;
    while i + 1 < listed.len() {
        if listed[i].0 == listed[i + 1].0 {
            let a = rank(&listed[i].1);
            let b = rank(&listed[i + 1].1);
            if b > a {
                listed[i].1 = listed[i + 1].1.clone();
            }
            listed.remove(i + 1);
        } else {
            i += 1;
        }
    }
}

enum PushDisplay {
    Plain,
    ForcedPlain,
    WithStatus(&'static str),
    ForcedWithStatus(&'static str),
}

fn shorten_remote_branch_display(full: &str) -> String {
    full.strip_prefix("refs/remotes/")
        .map(|s| s.to_owned())
        .unwrap_or_else(|| full.to_owned())
}

/// Abbreviate a ref name like Git's `abbrev_branch`: strip a leading `refs/heads/` or
/// `refs/remotes/` prefix, leaving other names untouched.
fn abbrev_branch(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .unwrap_or(name)
}

fn local_branches_for_pull(config: &ConfigSet, remote: &str) -> Vec<(String, bool, Vec<String>)> {
    let mut out: Vec<(String, bool, Vec<String>)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for e in config.entries() {
        let p: Vec<&str> = e.key.splitn(3, '.').collect();
        if p.len() != 3 || p[0] != "branch" {
            continue;
        }
        let branch = p[1].to_owned();
        if !seen.insert(branch.clone()) {
            continue;
        }
        let r = config
            .get(&format!("branch.{branch}.remote"))
            .unwrap_or_default();
        if r != remote {
            continue;
        }
        let raw_merges = config.get_all(&format!("branch.{branch}.merge"));
        if raw_merges.is_empty() {
            continue;
        }
        // Git splits each `branch.<n>.merge` value on spaces and abbreviates every token
        // (see `config_read_branches`), so "topic-a topic-b topic-c" becomes three entries.
        let merges: Vec<String> = raw_merges
            .iter()
            .flat_map(|v| v.split(' ').filter(|s| !s.is_empty()))
            .map(|s| abbrev_branch(s).to_owned())
            .collect();
        let rebase = config
            .get(&format!("branch.{branch}.rebase"))
            .map(|v| {
                let l = v.to_ascii_lowercase();
                l == "true" || l == "1" || l == "yes" || l == "interactive" || l == "merges"
            })
            .unwrap_or(false);
        out.push((branch, rebase, merges));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn compute_push_status_lines(
    repo: &Repository,
    config: &ConfigSet,
    remote_name: &str,
    urls: &RemoteUrls,
) -> Result<Vec<(String, String, PushDisplay)>> {
    let url = urls.fetch.first().cloned().unwrap_or_default();
    let Some(rpath) = url_to_local_repo_path(&url) else {
        return Ok(Vec::new());
    };
    let remote_git = find_git_dir(&rpath)?;
    let mut remote_by_ref: HashMap<String, ObjectId> = HashMap::new();
    for (r, oid) in refs::list_refs(&remote_git, "refs/heads/")? {
        remote_by_ref.insert(r, oid);
    }

    let specs = config.get_all(&format!("remote.{remote_name}.push"));
    let mut out: Vec<(String, String, PushDisplay)> = Vec::new();

    if specs.is_empty() {
        for (local_ref, local_oid) in refs::list_refs(&repo.git_dir, "refs/heads/")? {
            let short = abbrev_branch_display(&local_ref);
            let dest_ref = local_ref.clone();
            let old = remote_by_ref.get(&dest_ref).copied();
            let st = classify_push(old, local_oid, repo)?;
            out.push((short, abbrev_branch_display(&dest_ref), st));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        return Ok(out);
    }

    for spec in specs {
        let (forced, s) = if let Some(r) = spec.strip_prefix('+') {
            (true, r)
        } else {
            (false, spec.as_str())
        };
        if s == ":" {
            // The matching (":") refspec only pushes local branches that already exist on the
            // remote (Git's `match_push_refs` with MATCH_REFS_NONE). Branches absent on the remote
            // are not advertised here.
            for (local_ref, local_oid) in refs::list_refs(&repo.git_dir, "refs/heads/")? {
                let dest_ref = local_ref.clone();
                let Some(old) = remote_by_ref.get(&dest_ref).copied() else {
                    continue;
                };
                let st = classify_push(Some(old), local_oid, repo)?;
                let st = if forced {
                    match st {
                        PushDisplay::Plain => PushDisplay::ForcedPlain,
                        PushDisplay::WithStatus(x) => PushDisplay::ForcedWithStatus(x),
                        _ => st,
                    }
                } else {
                    st
                };
                out.push((
                    abbrev_branch_display(&local_ref),
                    abbrev_branch_display(&dest_ref),
                    st,
                ));
            }
            continue;
        }
        if let Some((left, right)) = s.split_once(':') {
            let src = normalize_push_src(left);
            let dst = normalize_push_dest(right);
            let local_oid = refs::resolve_ref(&repo.git_dir, &src).ok();
            let Some(lo) = local_oid else {
                continue;
            };
            let old = remote_by_ref.get(&dst).copied();
            let mut st = classify_push(old, lo, repo)?;
            if forced {
                st = match st {
                    PushDisplay::Plain => PushDisplay::ForcedPlain,
                    PushDisplay::WithStatus(x) => PushDisplay::ForcedWithStatus(x),
                    _ => st,
                };
            }
            out.push((abbrev_branch_display(&src), abbrev_branch_display(&dst), st));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn normalize_push_src(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("refs/") {
        s.to_owned()
    } else {
        format!("refs/heads/{s}")
    }
}

fn normalize_push_dest(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("refs/") {
        s.to_owned()
    } else {
        format!("refs/heads/{s}")
    }
}

fn abbrev_branch_display(full: &str) -> String {
    full.strip_prefix("refs/heads/")
        .or_else(|| full.strip_prefix("refs/tags/"))
        .unwrap_or(full)
        .to_owned()
}

/// Classify the push state of one ref, mirroring Git's `get_push_ref_states`.
///
/// `old` is the OID the *remote* ref currently points to; `new` is the local OID that would be
/// pushed. Following Git, when the local object store does not contain `old`, the remote is ahead
/// and the status is "local out of date" rather than an error (the ancestry check is skipped).
fn classify_push(old: Option<ObjectId>, new: ObjectId, repo: &Repository) -> Result<PushDisplay> {
    if new == ObjectId::zero() {
        return Ok(PushDisplay::WithStatus("delete"));
    }
    let Some(o) = old else {
        return Ok(PushDisplay::WithStatus("create"));
    };
    if o == new {
        return Ok(PushDisplay::WithStatus("up to date"));
    }
    // Git only treats this as fast-forwardable when the local repo *has* the remote's old object
    // and `new` is strictly newer; otherwise the remote ref is ahead -> "local out of date".
    if repo.odb.exists(&o) && is_ancestor(repo, o, new)? {
        return Ok(PushDisplay::WithStatus("fast-forwardable"));
    }
    Ok(PushDisplay::WithStatus("local out of date"))
}

fn cmd_prune(rest: &[String]) -> Result<()> {
    let mut dry = false;
    let mut i = 0usize;
    while i < rest.len() {
        match rest[i].as_str() {
            "-n" | "--dry-run" => {
                dry = true;
                i += 1;
            }
            _ if rest[i].starts_with('-') => bail!("unknown option: {}", rest[i]),
            _ => break,
        }
    }
    let rest = &rest[i..];
    if rest.is_empty() {
        bail!("usage: git remote prune [-n] <name>...");
    }
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    for name in rest {
        prune_one(&git_dir, &config, name, dry)?;
    }
    Ok(())
}

/// Reverse-map a local ref through the positive fetch refspecs, returning every remote source name
/// that would produce it (Git's `refspec_find_all_matches` with `find_src`). Returns an empty list
/// when the local ref is shielded by a negative refspec or matches no refspec destination.
fn stale_candidate_sources(local_ref: &str, refspecs: &[FetchRefspec]) -> Vec<String> {
    if local_ref_protected_by_negative_show(local_ref, refspecs) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for rs in refspecs {
        if rs.negative || rs.dst.is_empty() {
            continue;
        }
        if let Some(src) = reverse_map_src(&rs.dst, &rs.src, local_ref) {
            out.push(src);
        }
    }
    out
}

/// Whether `local_ref` reverse-maps to a remote source caught by a negative refspec (so it must not
/// be treated as matching the refspec set at all). Mirrors `refspec_find_negative_match`.
fn local_ref_protected_by_negative_show(local_ref: &str, refspecs: &[FetchRefspec]) -> bool {
    if !refspecs.iter().any(|rs| rs.negative) {
        return false;
    }
    refspecs
        .iter()
        .filter(|rs| !rs.negative && !rs.dst.is_empty())
        .filter_map(|rs| reverse_map_src(&rs.dst, &rs.src, local_ref))
        .any(|src| ref_excluded_by_fetch_refspecs(&src, refspecs))
}

fn prune_one(git_dir: &Path, config: &ConfigSet, name: &str, dry: bool) -> Result<()> {
    if !remote_section_exists(config, name) {
        bail!("No such remote '{}'", name);
    }
    let url = config
        .get(&format!("remote.{name}.url"))
        .unwrap_or_default();
    let Some(path) = url_to_local_repo_path(&url) else {
        return Ok(());
    };
    let remote_git = find_git_dir(&path)?;
    // Source ref names currently advertised by the remote (all refs, not just heads — mirrors
    // fetch `refs/*:refs/*`).
    let advertised_srcs: HashSet<String> = refs::list_refs(&remote_git, "refs/")?
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    let fetch = build_remote_stub(config, name).fetch;

    // Stale = local refs that a fetch refspec maps from, whose remote source no longer exists.
    let mut stale: Vec<String> = Vec::new();
    for (local_ref, _) in refs::list_refs(git_dir, "refs/")? {
        // Symbolic refs (e.g. refs/remotes/<n>/HEAD) are never pruned here.
        if let Ok(Some(_)) = grit_lib::refs::read_symbolic_ref(git_dir, &local_ref) {
            continue;
        }
        let candidates = stale_candidate_sources(&local_ref, &fetch);
        if candidates.is_empty() {
            continue;
        }
        if !candidates.iter().any(|c| advertised_srcs.contains(c)) {
            stale.push(local_ref);
        }
    }
    stale.sort();
    if stale.is_empty() {
        return Ok(());
    }
    println!("Pruning {name}");
    println!("URL: {url}");
    // Capture symref targets before deletion so we can warn about any that become dangling.
    let symrefs: Vec<(String, String)> = refs::list_refs(git_dir, "refs/remotes/")?
        .into_iter()
        .filter_map(|(rn, _)| {
            grit_lib::refs::read_symbolic_ref(git_dir, &rn)
                .ok()
                .flatten()
                .map(|target| (rn, target))
        })
        .collect();
    let deleted: HashSet<&String> = stale.iter().collect();
    for r in &stale {
        let short = abbrev_branch(r);
        if dry {
            println!(" * [would prune] {short}");
        } else {
            refs::delete_ref(git_dir, r).with_context(|| format!("pruning {r}"))?;
            println!(" * [pruned] {short}");
        }
    }
    // Git's `refs_warn_dangling_symrefs`: any symref pointing at a (now) deleted ref is reported.
    for (sym_name, target) in &symrefs {
        if deleted.contains(target) {
            if dry {
                eprintln!("{sym_name} will become dangling after {target} is deleted");
            } else {
                eprintln!("{sym_name} has become dangling after {target} was deleted");
            }
        }
    }
    Ok(())
}

fn cmd_update(rest: &[String], verbose: bool) -> Result<()> {
    let mut prune: Option<bool> = None;
    let mut i = 0usize;
    while i < rest.len() {
        match rest[i].as_str() {
            "-p" | "--prune" => {
                prune = Some(true);
                i += 1;
            }
            "--no-prune" => {
                prune = Some(false);
                i += 1;
            }
            _ if rest[i].starts_with('-') => bail!("unknown option: {}", rest[i]),
            _ => break,
        }
    }
    let mut rest = rest[i..].to_vec();
    let git_dir = resolve_git_dir()?;
    let config = load_local_config(&git_dir)?;
    let has_default = config.get("remotes.default").is_some();
    if rest.is_empty() {
        if has_default {
            rest.push("default".to_owned());
        } else {
            for n in default_update_remotes(&config) {
                run_fetch_for_update(&n, verbose, prune)?;
            }
            return Ok(());
        }
    }
    for g in rest {
        if g == "default" && !has_default {
            for n in default_update_remotes(&config) {
                run_fetch_for_update(&n, verbose, prune)?;
            }
            continue;
        }
        if remote_section_exists(&config, &g) {
            run_fetch_for_update(&g, verbose, prune)?;
            continue;
        }
        let group_key = format!("remotes.{g}");
        let lines = config.get_all(&group_key);
        if lines.is_empty() {
            bail!("No such remote or remote group: '{}'", g);
        }
        for line in lines {
            for member in line.split_whitespace() {
                run_fetch_for_update(member, verbose, prune)?;
            }
        }
    }
    Ok(())
}

/// Remotes fetched by the implicit `default` update group: all configured remotes minus any with
/// `remote.<name>.skipDefaultUpdate` or `remote.<name>.skipFetchAll` set to a true value
/// (Git's `default` pseudo-group). Returned sorted to match `git remote update`'s ordering.
fn default_update_remotes(config: &ConfigSet) -> Vec<String> {
    let mut names = collect_remote_names_from_config(config);
    names.sort();
    names
        .into_iter()
        .filter(|n| {
            let skip = |key: &str| {
                config
                    .get(&format!("remote.{n}.{key}"))
                    .map(|v| {
                        let v = v.trim().to_ascii_lowercase();
                        v == "true" || v == "yes" || v == "1" || v == "on"
                    })
                    .unwrap_or(false)
            };
            !(skip("skipdefaultupdate") || skip("skipfetchall"))
        })
        .collect()
}

fn run_fetch_for_update(remote: &str, verbose: bool, prune: Option<bool>) -> Result<()> {
    let self_exe = std::env::current_exe().context("cannot determine own executable")?;
    let mut cmd = std::process::Command::new(&self_exe);
    cmd.arg("fetch");
    if let Some(p) = prune {
        if p {
            cmd.arg("--prune");
        } else {
            cmd.arg("--no-prune");
        }
    }
    if verbose {
        cmd.arg("-v");
    }
    cmd.arg(remote);
    println!("Fetching {remote}");
    let status = cmd.status().context("fetch failed")?;
    if !status.success() {
        bail!("Could not fetch {remote}");
    }
    Ok(())
}

/// Legacy clap entry (unused for argv; see [`run_from_argv`]).
pub fn run(_args: Args) -> Result<()> {
    cmd_list(_args.verbose)
}
