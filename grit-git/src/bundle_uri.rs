//! `--bundle-uri` support: download Git bundle files or bundle lists, unbundle into the ODB,
//! and record refs under `refs/bundles/`.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::connectivity::bundle_prerequisites_connected_to_refs;
use grit_lib::fsck_standalone::fsck_object;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use grit_lib::unpack_objects::{unpack_objects, UnpackOptions};
use url::Url;

use crate::http_bundle_uri::strip_v0_service_advertisement_if_present;
use grit_lib::pkt_line;

thread_local! {
    static HTTP_BUNDLE_CACHE: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
}

/// Clear cached HTTP bundle downloads (call once per top-level `fetch` / `clone` command).
pub fn clear_http_bundle_cache() {
    HTTP_BUNDLE_CACHE.with(|c| c.borrow_mut().clear());
}

fn validate_bundle_uri_token(uri: &str) -> Result<()> {
    if uri.contains('\n') || uri.contains('\r') {
        bail!("bundle-uri: URI is malformed: {uri}");
    }
    if uri.contains(' ') {
        bail!("bundle-uri: URI is malformed: {uri}");
    }
    Ok(())
}

fn validate_clone_directory_name(name: &str) -> Result<()> {
    if name.contains('\n') || name.contains('\r') {
        bail!("bundle-uri: filename is malformed: {name}");
    }
    Ok(())
}

/// Resolve a bundle entry `uri` against the bundle list document URL (Git `bundle-uri` behavior).
fn resolve_bundle_entry_uri(list_uri: &str, entry_uri: &str) -> String {
    let e = entry_uri.trim();
    if e.starts_with("http://")
        || e.starts_with("https://")
        || e.starts_with("file://")
        || e.starts_with('/')
    {
        return e.to_string();
    }
    if let Ok(base) = Url::parse(list_uri) {
        if let Ok(j) = base.join(e) {
            return j.into();
        }
    }
    e.to_string()
}

fn read_bundle_uri_bytes(uri: &str) -> Result<Vec<u8>> {
    validate_bundle_uri_token(uri)?;
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return HTTP_BUNDLE_CACHE.with(|c| {
            let mut map = c.borrow_mut();
            // Emit trace2 for every logical GET (including cache hits) so `GIT_TRACE2_EVENT`
            // matches Git when the same bundle URL is applied multiple times in one command.
            crate::http_smart::trace2_child_start_git_remote_https(uri);
            if let Some(b) = map.get(uri) {
                return Ok(b.clone());
            }
            let config = ConfigSet::load(None, true).unwrap_or_default();
            let client = crate::http_client::HttpClientContext::from_config_set(&config)?;
            let body = client
                .get_with_git_protocol(uri, None)
                .with_context(|| format!("failed to download bundle from URI '{uri}'"))?;
            map.insert(uri.to_string(), body.clone());
            Ok(body)
        });
    }
    let path = if let Some(p) = uri.strip_prefix("file://") {
        PathBuf::from(p)
    } else {
        PathBuf::from(uri)
    };
    fs::read(&path).with_context(|| format!("warning: failed to download bundle from URI '{uri}'"))
}

fn is_bundle_v2(data: &[u8]) -> bool {
    data.starts_with(b"# v2 git bundle\n")
}

#[allow(clippy::type_complexity)]
fn parse_bundle_header_refs(
    data: &[u8],
) -> Result<(Vec<(String, ObjectId)>, Vec<ObjectId>, usize)> {
    let header_line = b"# v2 git bundle\n";
    if !data.starts_with(header_line) {
        bail!("is not a bundle");
    }
    let mut pos = header_line.len();
    let mut refs: Vec<(String, ObjectId)> = Vec::new();
    let mut prerequisites: Vec<ObjectId> = Vec::new();
    loop {
        let eol = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .ok_or_else(|| anyhow::anyhow!("truncated bundle header"))?;
        let line = &data[pos..eol];
        if line.is_empty() {
            pos = eol + 1;
            break;
        }
        let line_str = std::str::from_utf8(line)?;
        if let Some(rest) = line_str.strip_prefix('-') {
            let hex = rest.split_whitespace().next().unwrap_or(rest).trim();
            if let Ok(oid) = ObjectId::from_hex(hex) {
                prerequisites.push(oid);
            }
            pos = eol + 1;
            continue;
        }
        if let Some((hex, refname)) = line_str.split_once(' ') {
            let oid =
                ObjectId::from_hex(hex).map_err(|e| anyhow::anyhow!("bad oid in bundle: {e}"))?;
            refs.push((refname.to_string(), oid));
        }
        pos = eol + 1;
    }
    Ok((refs, prerequisites, pos))
}

fn bundle_ref_under_bundles(refname: &str) -> String {
    if let Some(rest) = refname.strip_prefix("refs/") {
        format!("refs/bundles/{rest}")
    } else {
        format!("refs/bundles/heads/{refname}")
    }
}

fn fetch_fsck_objects(git_dir: &Path) -> bool {
    let set = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    set.get_bool("fetch.fsckobjects")
        .or_else(|| set.get_bool("fetch.fsckObjects"))
        .map(|b| b.unwrap_or(false))
        .unwrap_or(false)
}

fn maybe_fsck_unpacked_objects(git_dir: &Path, odb: &Odb, oids: &[ObjectId]) -> Result<()> {
    if !fetch_fsck_objects(git_dir) {
        return Ok(());
    }
    for oid in oids {
        let obj = odb
            .read(oid)
            .with_context(|| format!("fsck read {}", oid.to_hex()))?;
        if let Err(e) = fsck_object(obj.kind, &obj.data) {
            eprintln!(
                "error: object {} fails fsck: {}",
                oid.to_hex(),
                e.report_line()
            );
            bail!("missingEmail");
        }
    }
    Ok(())
}

fn unbundle_pack_into_repo(
    git_dir: &Path,
    data: &[u8],
    pack_start: usize,
    prerequisites: &[ObjectId],
    refs: &[(String, ObjectId)],
    skip_if_prereqs_missing: bool,
) -> Result<()> {
    let odb = Odb::new(&git_dir.join("objects"));
    for p in prerequisites {
        if !odb.exists(p) {
            eprintln!(
                "warning: skipping bundle: missing prerequisite object {}",
                p.to_hex()
            );
            if skip_if_prereqs_missing {
                return Ok(());
            }
            bail!(
                "bundle prerequisite {} missing from object database",
                p.to_hex()
            );
        }
    }
    let pack_data = &data[pack_start..];
    if pack_data.len() < 12 + 20 {
        return Ok(());
    }
    let opts = UnpackOptions {
        strict: false,
        dry_run: false,
        quiet: true,
        allowed_missing: Default::default(),
        allow_promisor_missing_references: false,
        max_input_bytes: None,
        ..Default::default()
    };
    let mut collected: Vec<ObjectId> = Vec::new();
    let before_ids: std::collections::HashSet<ObjectId> = list_all_loose_oids(&odb);
    unpack_objects(&mut &pack_data[..], &odb, &opts)
        .map_err(|e| anyhow::anyhow!("unbundle failed: {e}"))?;
    let after_ids = list_all_loose_oids(&odb);
    for id in after_ids {
        if !before_ids.contains(&id) {
            collected.push(id);
        }
    }
    maybe_fsck_unpacked_objects(git_dir, &odb, &collected)?;

    for (refname, oid) in refs {
        if refname == "HEAD" {
            continue;
        }
        let dest_name = bundle_ref_under_bundles(refname);
        if check_refname_format(&dest_name, &RefNameOptions::default()).is_err() {
            continue;
        }
        if odb.read(oid).is_err() {
            eprintln!(
                "error: trying to write ref '{dest_name}' with nonexistent object {}",
                oid.to_hex()
            );
            continue;
        }
        let p = git_dir.join(&dest_name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&p, format!("{}\n", oid.to_hex()))?;
    }
    Ok(())
}

fn list_all_loose_oids(odb: &Odb) -> std::collections::HashSet<ObjectId> {
    let mut out = std::collections::HashSet::new();
    let root = odb.objects_dir();
    let Ok(rd) = fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(inner) = fs::read_dir(e.path()) else {
            continue;
        };
        for f in inner.flatten() {
            let stem = f.file_name().to_string_lossy().to_string();
            if stem.len() != 38 {
                continue;
            }
            let hex = format!("{name}{stem}");
            if let Ok(id) = ObjectId::from_hex(&hex) {
                out.insert(id);
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
struct BundleListEntry {
    id: String,
    uri: String,
    creation_token: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleMode {
    All,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleHeuristic {
    None,
    CreationToken,
}

fn parse_bundle_list_ini(
    text: &str,
) -> Result<(BundleMode, BundleHeuristic, Vec<BundleListEntry>)> {
    let mut mode = BundleMode::All;
    let mut heuristic = BundleHeuristic::None;
    let mut entries: Vec<BundleListEntry> = Vec::new();
    let mut section: Option<String> = None;
    let mut current_uri: Option<String> = None;
    let mut current_token: Option<u64> = None;

    let flush_stanza = |entries: &mut Vec<BundleListEntry>,
                        section: &Option<String>,
                        uri: &mut Option<String>,
                        token: &mut Option<u64>| {
        let Some(id) = section.as_ref() else {
            uri.take();
            token.take();
            return;
        };
        if let Some(u) = uri.take() {
            entries.push(BundleListEntry {
                id: id.clone(),
                uri: u,
                creation_token: token.take(),
            });
        } else {
            token.take();
        }
    };

    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            flush_stanza(&mut entries, &section, &mut current_uri, &mut current_token);
            if line == "[bundle]" {
                section = None;
            } else if let Some(inner) = line
                .strip_prefix("[bundle \"")
                .and_then(|s| s.strip_suffix("\"]"))
            {
                section = Some(inner.to_string());
                current_uri = None;
                current_token = None;
            }
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if section.is_none() {
                match k {
                    "mode" => {
                        mode = if v == "any" {
                            BundleMode::Any
                        } else {
                            BundleMode::All
                        };
                    }
                    "heuristic" if v.eq_ignore_ascii_case("creationtoken") => {
                        heuristic = BundleHeuristic::CreationToken;
                    }
                    _ => {}
                }
            } else if k == "uri" {
                current_uri = Some(v.to_string());
            } else if k == "creationToken" || k.eq_ignore_ascii_case("creationtoken") {
                current_token = v.parse().ok();
            }
        }
    }
    flush_stanza(&mut entries, &section, &mut current_uri, &mut current_token);
    Ok((mode, heuristic, entries))
}

fn is_bundle_list_config(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim();
        t.starts_with("[bundle") && t.contains(']')
    })
}

fn apply_single_bundle_file(git_dir: &Path, data: &[u8]) -> Result<()> {
    if !is_bundle_v2(data) {
        bail!("is not a bundle");
    }
    let (refs, prerequisites, pack_start) = parse_bundle_header_refs(data)?;
    unbundle_pack_into_repo(git_dir, data, pack_start, &prerequisites, &refs, true)?;
    Ok(())
}

fn apply_single_bundle_file_strict_prereqs(git_dir: &Path, data: &[u8]) -> Result<()> {
    if !is_bundle_v2(data) {
        bail!("is not a bundle");
    }
    let (refs, prerequisites, pack_start) = parse_bundle_header_refs(data)?;
    unbundle_pack_into_repo(git_dir, data, pack_start, &prerequisites, &refs, false)?;
    Ok(())
}

fn apply_bundle_from_uri(git_dir: &Path, uri: &str) -> Result<()> {
    let bytes = read_bundle_uri_bytes(uri)?;
    apply_single_bundle_file(git_dir, &bytes)
}

fn apply_bundle_list(
    git_dir: &Path,
    list_uri: &str,
    _list_text: &str,
    mode: BundleMode,
    _heuristic: BundleHeuristic,
    entries: &[BundleListEntry],
) -> Result<()> {
    let mut any_ok = false;
    for e in entries {
        let resolved = resolve_bundle_entry_uri(list_uri, &e.uri);
        match apply_bundle_from_uri(git_dir, &resolved) {
            Ok(()) => {
                any_ok = true;
            }
            Err(err) => {
                let chain_text: String = err
                    .chain()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(": ");
                if chain_text.contains("is not a bundle") {
                    bail!("{}", err);
                }
                // Match `git bundle-uri` stderr (t5558 greps this exact line for HTTP 404, etc.).
                eprintln!("warning: failed to download bundle from URI '{}'", resolved);
            }
        }
    }
    if mode == BundleMode::Any && !any_ok {
        eprintln!("warning: bundle list did not apply any bundles");
    }
    Ok(())
}

fn set_fetch_bundle_config(git_dir: &Path, list_uri: &str, token: Option<u64>) -> Result<()> {
    let path = git_dir.join("config");
    let mut cfg = match ConfigFile::from_path(&path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&path, "", ConfigScope::Local)?,
    };
    cfg.set("fetch.bundleURI", list_uri)?;
    cfg.set("fetch.bundleuri", list_uri)?;
    if let Some(t) = token {
        let s = t.to_string();
        cfg.set("fetch.bundleCreationToken", &s)?;
        cfg.set("fetch.bundlecreationtoken", &s)?;
    }
    cfg.write().context("writing fetch.bundle config")?;
    Ok(())
}

fn clear_fetch_bundle_config(git_dir: &Path) -> Result<()> {
    let path = git_dir.join("config");
    let mut cfg = match ConfigFile::from_path(&path, ConfigScope::Local)? {
        Some(c) => c,
        None => return Ok(()),
    };
    let _ = cfg.unset("fetch.bundleURI");
    let _ = cfg.unset("fetch.bundleuri");
    let _ = cfg.unset("fetch.bundleCreationToken");
    let _ = cfg.unset("fetch.bundlecreationtoken");
    cfg.write().ok();
    Ok(())
}

fn apply_creation_token_heuristic_clone(
    git_dir: &Path,
    list_uri: &str,
    list_text: &str,
    entries: &[BundleListEntry],
) -> Result<()> {
    let mut by_token: BTreeMap<u64, Vec<&BundleListEntry>> = BTreeMap::new();
    for e in entries {
        if let Some(t) = e.creation_token {
            by_token.entry(t).or_default().push(e);
        }
    }
    if by_token.is_empty() {
        return apply_bundle_list(
            git_dir,
            list_uri,
            list_text,
            BundleMode::All,
            BundleHeuristic::None,
            entries,
        );
    }

    // HTTP(S) lists: match `git fetch_bundles_by_token` (download high creationToken first;
    // t5558 `GIT_TRACE2_EVENT` ordering).
    if list_uri.starts_with("http://") || list_uri.starts_with("https://") {
        return fetch_bundles_by_token(git_dir, list_uri, entries);
    }

    let tokens: Vec<u64> = by_token.keys().copied().collect();
    let mut expect: u64 = 1;
    let mut max_contiguous: Option<u64> = None;

    for (tok, group) in &by_token {
        if *tok != expect {
            break;
        }
        let mut group_ok = true;
        for e in group {
            let resolved = resolve_bundle_entry_uri(list_uri, &e.uri);
            match apply_bundle_from_uri(git_dir, &resolved) {
                Ok(()) => {}
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("is not a bundle") {
                        bail!("{msg}");
                    }
                    eprintln!("warning: failed to download bundle from URI '{resolved}'");
                    group_ok = false;
                }
            }
        }
        if !group_ok {
            break;
        }
        max_contiguous = Some(*tok);
        expect = tok.saturating_add(1);
    }

    if max_contiguous == tokens.last().copied() {
        if let Some(t) = max_contiguous {
            set_fetch_bundle_config(git_dir, list_uri, Some(t))?;
        }
    } else {
        clear_fetch_bundle_config(git_dir)?;
    }

    Ok(())
}

/// Apply `--bundle-uri` after the destination repository exists (objects directory ready).
///
/// When `relax_http_download_failures` is true (local clone), HTTP GET failures are logged and
/// the clone continues. When false (HTTP clone), they abort the operation.
pub fn apply_bundle_uri(
    git_dir: &Path,
    bundle_uri: &str,
    directory_for_validation: &str,
    relax_http_download_failures: bool,
) -> Result<()> {
    if let Err(e) = (|| -> Result<()> {
        validate_clone_directory_name(directory_for_validation)?;
        validate_bundle_uri_token(bundle_uri)?;

        let bytes = read_bundle_uri_bytes(bundle_uri)?;
        let text = String::from_utf8_lossy(&bytes);
        if is_bundle_list_config(&text) {
            let (mode, heuristic, entries) = parse_bundle_list_ini(&text)?;
            if heuristic == BundleHeuristic::CreationToken {
                apply_creation_token_heuristic_clone(git_dir, bundle_uri, &text, &entries)
            } else {
                apply_bundle_list(git_dir, bundle_uri, &text, mode, heuristic, &entries)
            }
        } else {
            apply_single_bundle_file(git_dir, &bytes)?;
            Ok(())
        }
    })() {
        let chain_text: String = e
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(": ");
        if relax_http_download_failures {
            let msg = e.to_string();
            let malformed_uri = chain_text.contains("bundle-uri: URI is malformed")
                || msg.contains("bundle-uri: URI is malformed");
            let malformed_name = chain_text.contains("bundle-uri: filename is malformed")
                || msg.contains("bundle-uri: filename is malformed");
            if malformed_uri || malformed_name {
                if msg.starts_with("error:") {
                    eprintln!("{msg}");
                } else {
                    eprintln!("error: {msg}");
                }
                return Ok(());
            }
            if chain_text.contains("warning: failed to download bundle")
                || msg.contains("warning: failed to download bundle")
                || chain_text.contains("failed to download bundle from URI")
                || msg.contains("failed to download bundle from URI")
            {
                eprintln!("{}", e);
                return Ok(());
            }
        }
        if chain_text.contains("missingEmail") {
            return Ok(());
        }
        if chain_text.contains("is not a bundle") {
            eprintln!("error: is not a bundle");
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

fn read_bundle_list_from_http(
    repo_url: &str,
    client_override: Option<&crate::http_client::HttpClientContext>,
) -> Result<String> {
    let original_base = repo_url.trim_end_matches('/');
    let mut refs_url = format!("{original_base}/info/refs");
    refs_url.push_str(if refs_url.contains('?') { "&" } else { "?" });
    refs_url.push_str("service=git-upload-pack");

    let config = ConfigSet::load(None, true).unwrap_or_default();
    let owned_client;
    let client = if let Some(client) = client_override {
        client
    } else {
        owned_client = crate::http_client::HttpClientContext::from_config_set(&config)?;
        &owned_client
    };
    // Re-base onto a redirected `info/refs` location so the bundle-uri POST
    // targets the right host (shared with the fetch path via grit-lib).
    let (body, base) = crate::http_smart::http_get_discovery_with_base(
        client,
        &refs_url,
        original_base,
        Some("version=2"),
    )?;
    let pkt_body = strip_v0_service_advertisement_if_present(&body)?;
    let mut cur = Cursor::new(pkt_body);
    let first = match pkt_line::read_packet(&mut cur)? {
        None => bail!("empty v2 capability block"),
        Some(pkt_line::Packet::Data(s)) => s,
        Some(other) => bail!("expected version line, got {other:?}"),
    };
    if first != "version 2" {
        bail!("expected 'version 2', got {first:?}");
    }
    let mut caps = vec![first];
    loop {
        match pkt_line::read_packet(&mut cur)? {
            None => bail!("unexpected EOF in v2 capabilities"),
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(s)) => caps.push(s),
            Some(other) => bail!("unexpected packet in v2 caps: {other:?}"),
        }
    }
    if !caps
        .iter()
        .any(|c| c == "bundle-uri" || c.starts_with("bundle-uri="))
    {
        bail!("server does not advertise bundle-uri");
    }

    let mut cap_send = Vec::new();
    for line in &caps {
        if line.starts_with("agent=") {
            cap_send.push(line.clone());
        } else if let Some(fmt) = line.strip_prefix("object-format=") {
            cap_send.push(format!("object-format={fmt}"));
        }
    }

    let mut request = Vec::new();
    pkt_line::write_line_to_vec(&mut request, "command=bundle-uri")?;
    for line in &cap_send {
        pkt_line::write_line_to_vec(&mut request, line)?;
    }
    pkt_line::write_delim(&mut request)?;
    pkt_line::write_flush(&mut request)?;

    let post_url = format!("{base}/git-upload-pack");
    let out_body = client
        .post_with_git_protocol(
            &post_url,
            "application/x-git-upload-pack-request",
            "application/x-git-upload-pack-result",
            &request,
            Some("version=2"),
        )
        .with_context(|| format!("POST {post_url}"))?;

    let mut ini = String::new();
    ini.push_str("[bundle]\n\tversion = 1\n\tmode = all\n");
    let mut cur2 = Cursor::new(&out_body);
    loop {
        match pkt_line::read_packet(&mut cur2)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let (k, v) = line
                    .split_once('=')
                    .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("malformed bundle-uri line: {line}"))?;
                if let Some(rest) = k.strip_prefix("bundle.") {
                    if let Some((id, subkey)) = rest.rsplit_once('.') {
                        if subkey == "uri" {
                            ini.push_str(&format!("[bundle \"{id}\"]\n\turi = {v}\n"));
                        } else if subkey.eq_ignore_ascii_case("creationtoken")
                            || subkey == "creationToken"
                        {
                            ini.push_str(&format!("\tcreationToken = {v}\n"));
                        }
                    } else if k == "bundle.mode" {
                        ini.push_str(&format!("\tmode = {v}\n"));
                    } else if k == "bundle.heuristic" {
                        ini.push_str(&format!("\theuristic = {v}\n"));
                    }
                }
            }
            Some(other) => bail!("unexpected bundle-uri response packet: {other:?}"),
        }
    }
    Ok(ini)
}

fn load_bundle_list_document(uri_or_ini: &str) -> Result<String> {
    if is_bundle_list_config(uri_or_ini) {
        return Ok(uri_or_ini.to_string());
    }
    let bytes = read_bundle_uri_bytes(uri_or_ini)?;
    String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("bundle list is not valid UTF-8: {e}"))
}

fn resolve_bundle_uri_for_fetch(
    git_dir: &Path,
    remote_url: &str,
    bundle_uri_opt: Option<&str>,
    client_override: Option<&crate::http_client::HttpClientContext>,
) -> Result<String> {
    if let Some(u) = bundle_uri_opt {
        if !u.is_empty() {
            return load_bundle_list_document(u);
        }
    }
    let set = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    if let Some(u) = set
        .get("fetch.bundleURI")
        .or_else(|| set.get("fetch.bundleuri"))
    {
        return load_bundle_list_document(&u);
    }
    if remote_url.starts_with("http://") || remote_url.starts_with("https://") {
        return read_bundle_list_from_http(remote_url, client_override);
    }
    bail!("no bundle-uri configured for fetch");
}

fn config_i64(git_dir: &Path, key: &str) -> Option<i64> {
    let set = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    set.get(key).and_then(|s| s.parse().ok())
}

fn bundle_list_uri_for_config(git_dir: &Path, remote_url: &str) -> String {
    let set = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    set.get("fetch.bundleURI")
        .or_else(|| set.get("fetch.bundleuri"))
        .unwrap_or_else(|| remote_url.to_string())
}

#[derive(Debug)]
struct TokenBundleWork {
    token: u64,
    uri: String,
    /// `None` = not downloaded yet; `Some(Ok(bytes))` = downloaded; `Some(Err)` = download failed.
    file: Option<Result<Vec<u8>, ()>>,
    unbundled: bool,
}

/// Returns `true` when unbundle failed (Git: non-zero from `unbundle_from_file`).
///
/// Missing prerequisites must count as failure for `fetch_bundles_by_token` so the client keeps
/// downloading lower `creationToken` bundles. Plain `apply_single_bundle_file` skips missing
/// prerequisites with a warning and returns `Ok` so HTTP clones can fetch objects afterward.
fn unbundle_from_bytes(git_dir: &Path, data: &[u8]) -> bool {
    if !is_bundle_v2(data) {
        return true;
    }
    let Ok((_, prerequisites, _)) = parse_bundle_header_refs(data) else {
        return true;
    };
    let odb = Odb::new(&git_dir.join("objects"));
    for p in &prerequisites {
        if !odb.exists(p) {
            eprintln!(
                "warning: skipping bundle: missing prerequisite object {}",
                p.to_hex()
            );
            return true;
        }
    }
    if let Ok(repo) = Repository::open(git_dir, None) {
        match bundle_prerequisites_connected_to_refs(&repo, &prerequisites) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                eprintln!(
                    "error: some prerequisite commits exist in the object store, but are not connected to the repository's history"
                );
                return true;
            }
        }
    }
    apply_single_bundle_file_strict_prereqs(git_dir, data).is_err()
}

/// Port of Git's `fetch_bundles_by_token` (`bundle-uri.c`).
fn fetch_bundles_by_token(
    git_dir: &Path,
    list_uri: &str,
    entries: &[BundleListEntry],
) -> Result<()> {
    let max_creation_token: u64 = config_i64(git_dir, "fetch.bundleCreationToken")
        .or_else(|| config_i64(git_dir, "fetch.bundlecreationtoken"))
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0);

    let mut bundles: Vec<TokenBundleWork> = Vec::new();
    for e in entries {
        if let Some(t) = e.creation_token {
            bundles.push(TokenBundleWork {
                token: t,
                uri: e.uri.clone(),
                file: None,
                unbundled: false,
            });
        }
    }
    if bundles.is_empty() {
        return Ok(());
    }
    bundles.sort_by_key(|b| b.token);
    bundles.reverse();

    if bundles[0].token <= max_creation_token {
        return Ok(());
    }

    let n = bundles.len() as i32;
    let mut cur: i32 = 0;
    let mut move_direction: i32 = 0;
    let mut new_max_creation_token: u64 = 0;

    while cur >= 0 && cur < n {
        let idx = cur as usize;
        if bundles[idx].token <= max_creation_token {
            break;
        }

        if bundles[idx].file.is_none() {
            let resolved = resolve_bundle_entry_uri(list_uri, &bundles[idx].uri);
            let dl = read_bundle_uri_bytes(&resolved).map_err(|_| ());
            bundles[idx].file = Some(dl);
            match bundles[idx].file.as_ref() {
                Some(Ok(data)) => {
                    if !is_bundle_v2(data) {
                        let resolved = resolve_bundle_entry_uri(list_uri, &bundles[idx].uri);
                        eprintln!(
                            "warning: file downloaded from '{}' is not a bundle",
                            resolved
                        );
                        break;
                    }
                }
                _ => {
                    bundles[idx].unbundled = true;
                    move_direction = 1;
                    cur += move_direction;
                    continue;
                }
            }
        }

        let downloaded = matches!(bundles[idx].file.as_ref(), Some(Ok(_)));
        if downloaded && !bundles[idx].unbundled {
            let data = match bundles[idx].file.as_ref() {
                Some(Ok(bytes)) => bytes.as_slice(),
                _ => &[],
            };
            if unbundle_from_bytes(git_dir, data) {
                move_direction = 1;
            } else {
                move_direction = -1;
                bundles[idx].unbundled = true;
                if bundles[idx].token > new_max_creation_token {
                    new_max_creation_token = bundles[idx].token;
                }
            }
        }

        cur += move_direction;
    }

    if cur < 0 {
        set_fetch_bundle_config(git_dir, list_uri, Some(new_max_creation_token))?;
    }

    Ok(())
}

fn apply_bundle_list_for_fetch(
    git_dir: &Path,
    list_uri: &str,
    list_text: &str,
    mode: BundleMode,
    heuristic: BundleHeuristic,
    entries: &[BundleListEntry],
) -> Result<()> {
    if heuristic == BundleHeuristic::CreationToken {
        return fetch_bundles_by_token(git_dir, list_uri, entries);
    }

    apply_bundle_list(git_dir, list_uri, list_text, mode, heuristic, entries)
}

/// After an HTTP fetch, apply bundle-uri list from config or remote (protocol v2).
pub fn maybe_apply_bundle_uri_after_http_fetch(
    git_dir: &Path,
    remote_url: &str,
    bundle_uri_override: Option<&str>,
) -> Result<()> {
    maybe_apply_bundle_uri_after_http_fetch_with_client(
        git_dir,
        remote_url,
        bundle_uri_override,
        None,
    )
}

/// After an HTTP fetch, apply bundle-uri using an existing HTTP client when available.
pub fn maybe_apply_bundle_uri_after_http_fetch_with_client(
    git_dir: &Path,
    remote_url: &str,
    bundle_uri_override: Option<&str>,
    client: Option<&crate::http_client::HttpClientContext>,
) -> Result<()> {
    let list_text =
        match resolve_bundle_uri_for_fetch(git_dir, remote_url, bundle_uri_override, client) {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("no bundle-uri configured")
                    || msg.contains("server does not advertise bundle-uri")
                {
                    return Ok(());
                }
                return Err(e);
            }
        };
    let list_uri = bundle_list_uri_for_config(git_dir, remote_url);
    let (mode, heuristic, entries) = parse_bundle_list_ini(&list_text)?;
    apply_bundle_list_for_fetch(git_dir, &list_uri, &list_text, mode, heuristic, &entries)
}
