//! The "promisor-remote" protocol v2 capability (see `gitprotocol-v2(5)` and
//! `Documentation/config/promisor.adoc`).
//!
//! A server that uses promisor remotes (large-object promisors / LOPs) can advertise them to a
//! client, which may then accept some of them and lazily fetch the omitted objects from those
//! remotes directly instead of forcing the server to back-fill and serve them.
//!
//! This module provides Git-compatible promisor-remote handling:
//!
//! - [`promisor_remote_info`] builds the server's advertisement from config
//!   (`promisor.advertise`, `promisor.sendFields`, `remote.<name>.{url,partialCloneFilter,token}`).
//! - [`promisor_remote_reply`] is the client side: given the server advertisement and the client's
//!   config (`promisor.acceptFromServer`, `promisor.checkFields`), decide which advertised remotes
//!   to accept and produce the reply string.

use crate::config::ConfigSet;

/// One advertised or configured promisor remote.
#[derive(Debug, Clone, Default)]
pub struct PromisorInfo {
    pub name: String,
    pub url: Option<String>,
    pub filter: Option<String>,
    pub token: Option<String>,
}

const FIELD_FILTER: &str = "partialclonefilter";
const FIELD_TOKEN: &str = "token";

/// Whether `field` (case-insensitive) is one of the optional fields we understand.
fn is_known_field(field: &str) -> bool {
    let f = field.to_ascii_lowercase();
    f == FIELD_FILTER || f == FIELD_TOKEN
}

/// Parse a comma/space separated `promisor.{send,check,store}Fields` list into normalized
/// (lowercased) known field names, in order, dropping unknown fields.
fn parse_fields_config(cfg: &ConfigSet, key: &str) -> Vec<String> {
    let Some(raw) = cfg.get(key) else {
        return Vec::new();
    };
    raw.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| is_known_field(s))
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// URL-encode using Git's `allow_unsanitized` predicate (`promisor-remote.c`): only `,`, `;`, `%`
/// and non-printable / non-ASCII bytes are percent-encoded; everything else passes through.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let allow = b != b',' && b != b';' && b != b'%' && b > 32 && b < 127;
        if allow {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Percent-decode a field value (matches Git's `url_percent_decode`).
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// All promisor remotes configured on this repository, in config order, that also have a non-empty
/// `remote.<name>.url`. Matching Git's `promisor_remote_config`, a remote is a promisor remote when
/// `remote.<name>.promisor` is true OR `remote.<name>.partialCloneFilter` is set. Each remote's
/// `partialCloneFilter` and `token` are populated only when listed in `field_names`.
fn config_info_list(cfg: &ConfigSet, field_names: &[String]) -> Vec<PromisorInfo> {
    // Discover promisor remote names in first-seen config order.
    let mut names: Vec<String> = Vec::new();
    for e in cfg.entries() {
        let Some(rest) = e.key.strip_prefix("remote.") else {
            continue;
        };
        let is_promisor = rest
            .strip_suffix(".promisor")
            .and_then(|name| {
                let val = e.value.clone().unwrap_or_else(|| "true".to_owned());
                val.eq_ignore_ascii_case("true").then(|| name.to_string())
            })
            .or_else(|| {
                rest.strip_suffix(".partialclonefilter")
                    .map(|name| name.to_string())
            });
        if let Some(name) = is_promisor {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }

    let mut out = Vec::new();
    for name in names {
        let url = cfg.get(&format!("remote.{name}.url"));
        // Only advertise remotes with a non-empty URL.
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            continue;
        };

        let mut info = PromisorInfo {
            name: name.clone(),
            url: Some(url),
            ..Default::default()
        };
        for field in field_names {
            let key = format!("remote.{name}.{field}");
            if let Some(v) = cfg.get(&key).filter(|v| !v.is_empty()) {
                match field.as_str() {
                    FIELD_FILTER => info.filter = Some(v),
                    FIELD_TOKEN => info.token = Some(v),
                    _ => {}
                }
            }
        }
        out.push(info);
    }
    out
}

/// Build the server's `promisor-remote=<info>` advertisement value, or `None` when
/// `promisor.advertise` is not true or there are no advertisable promisor remotes.
#[must_use]
pub fn promisor_remote_info(cfg: &ConfigSet) -> Option<String> {
    let advertise = cfg
        .get_bool("promisor.advertise")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    if !advertise {
        return None;
    }

    let fields = parse_fields_config(cfg, "promisor.sendFields");
    let list = config_info_list(cfg, &fields);
    if list.is_empty() {
        return None;
    }

    let mut sb = String::new();
    for (i, p) in list.iter().enumerate() {
        if i != 0 {
            sb.push(';');
        }
        sb.push_str("name=");
        sb.push_str(&urlencode(&p.name));
        sb.push_str(",url=");
        sb.push_str(&urlencode(p.url.as_deref().unwrap_or("")));
        if let Some(f) = &p.filter {
            sb.push_str(",partialCloneFilter=");
            sb.push_str(&urlencode(f));
        }
        if let Some(t) = &p.token {
            sb.push_str(",token=");
            sb.push_str(&urlencode(t));
        }
    }
    Some(sb)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Accept {
    None,
    KnownUrl,
    KnownName,
    All,
}

fn parse_accept(cfg: &ConfigSet) -> Accept {
    match cfg.get("promisor.acceptFromServer") {
        Some(s) if s.eq_ignore_ascii_case("knownurl") => Accept::KnownUrl,
        Some(s) if s.eq_ignore_ascii_case("knownname") => Accept::KnownName,
        Some(s) if s.eq_ignore_ascii_case("all") => Accept::All,
        _ => Accept::None,
    }
}

/// Parse one `pr-fields` element (e.g. `name=lop,url=...,partialCloneFilter=...`) from the
/// server advertisement. Returns `None` when the mandatory `name`/`url` are missing.
fn parse_one_advertised(remote_info: &str) -> Option<PromisorInfo> {
    let mut info = PromisorInfo::default();
    for elem in remote_info.split(',') {
        let Some(eq) = elem.find('=') else {
            continue;
        };
        let (field, value) = (&elem[..eq], &elem[eq + 1..]);
        let value = urldecode(value);
        match field {
            "name" => info.name = value,
            "url" => info.url = Some(value),
            "partialCloneFilter" => info.filter = Some(value),
            "token" => info.token = Some(value),
            _ => {}
        }
    }
    if info.name.is_empty() || info.url.is_none() {
        return None;
    }
    Some(info)
}

/// Does the advertised remote satisfy `promisor.checkFields`? Each checked field must be
/// advertised and match the value configured locally for the corresponding remote.
fn all_fields_match(
    advertised: &PromisorInfo,
    config_info: &[PromisorInfo],
    checked: &[String],
    in_list: bool,
) -> bool {
    for field in checked {
        let adv_value = match field.as_str() {
            FIELD_FILTER => advertised.filter.as_deref(),
            FIELD_TOKEN => advertised.token.as_deref(),
            _ => None,
        };
        let Some(adv_value) = adv_value else {
            return false;
        };
        let matches = if in_list {
            config_info
                .iter()
                .any(|p| field_matches_config(field, adv_value, p))
        } else {
            config_info
                .iter()
                .find(|p| p.name == advertised.name)
                .is_some_and(|p| field_matches_config(field, adv_value, p))
        };
        if !matches {
            return false;
        }
    }
    true
}

fn field_matches_config(field: &str, value: &str, p: &PromisorInfo) -> bool {
    match field {
        FIELD_FILTER => p.filter.as_deref() == Some(value),
        FIELD_TOKEN => p.token.as_deref() == Some(value),
        _ => false,
    }
}

fn should_accept(
    accept: Accept,
    advertised: &PromisorInfo,
    config_info: &[PromisorInfo],
    checked: &[String],
) -> bool {
    if accept == Accept::All {
        return all_fields_match(advertised, config_info, checked, true);
    }

    let Some(local) = config_info.iter().find(|p| p.name == advertised.name) else {
        return false;
    };

    if accept == Accept::KnownName {
        return all_fields_match(advertised, config_info, checked, false);
    }

    // KnownUrl
    let adv_url = advertised.url.as_deref().unwrap_or("");
    if adv_url.is_empty() {
        return false;
    }
    if local.url.as_deref() == Some(adv_url) {
        return all_fields_match(advertised, config_info, checked, false);
    }
    false
}

/// The client's reply to the server's `promisor-remote` advertisement.
pub struct PromisorReply {
    /// `promisor-remote=<names>` value to send back, or `None` to send nothing.
    pub reply: Option<String>,
    /// Names of accepted remotes (decoded), in advertisement order.
    pub accepted: Vec<String>,
    /// For each accepted remote, its advertised `partialCloneFilter` (if any).
    pub accepted_filters: Vec<(String, String)>,
}

/// Parse a full server advertisement (`;`-separated `pr-fields`) into [`PromisorInfo`] entries,
/// dropping any that lack a name or URL.
#[must_use]
pub fn parse_advertisement(info: &str) -> Vec<PromisorInfo> {
    info.split(';').filter_map(parse_one_advertised).collect()
}

/// Client side: decide which advertised promisor remotes to accept and build the reply.
///
/// `info` is the raw value of the server's `promisor-remote=` capability.
#[must_use]
pub fn promisor_remote_reply(cfg: &ConfigSet, info: &str) -> PromisorReply {
    let accept = parse_accept(cfg);
    if accept == Accept::None {
        return PromisorReply {
            reply: None,
            accepted: Vec::new(),
            accepted_filters: Vec::new(),
        };
    }

    let checked = parse_fields_config(cfg, "promisor.checkFields");
    let config_info = config_info_list(cfg, &checked);

    let mut accepted = Vec::new();
    let mut accepted_filters = Vec::new();
    for elem in info.split(';') {
        let Some(advertised) = parse_one_advertised(elem) else {
            continue;
        };
        if should_accept(accept, &advertised, &config_info, &checked) {
            if let Some(f) = &advertised.filter {
                accepted_filters.push((advertised.name.clone(), f.clone()));
            }
            accepted.push(advertised.name);
        }
    }

    let reply = if accepted.is_empty() {
        None
    } else {
        Some(
            accepted
                .iter()
                .map(|n| urlencode(n))
                .collect::<Vec<_>>()
                .join(";"),
        )
    };

    PromisorReply {
        reply,
        accepted,
        accepted_filters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> ConfigSet {
        let mut c = ConfigSet::default();
        for (k, v) in pairs {
            c.add_command_override(k, v).unwrap();
        }
        c
    }

    #[test]
    fn no_advertise_when_disabled() {
        let c = cfg(&[
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        assert_eq!(promisor_remote_info(&c), None);
    }

    #[test]
    fn advertise_name_and_url() {
        let c = cfg(&[
            ("promisor.advertise", "true"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        assert_eq!(
            promisor_remote_info(&c).as_deref(),
            Some("name=lop,url=file:///lop")
        );
    }

    #[test]
    fn url_space_encoded() {
        let c = cfg(&[
            ("promisor.advertise", "true"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///a b"),
        ]);
        assert_eq!(
            promisor_remote_info(&c).as_deref(),
            Some("name=lop,url=file:///a%20b")
        );
    }

    #[test]
    fn advertise_send_fields() {
        let c = cfg(&[
            ("promisor.advertise", "true"),
            ("promisor.sendFields", "partialCloneFilter, token"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
            ("remote.lop.partialCloneFilter", "blob:none"),
            ("remote.lop.token", "fooBar"),
        ]);
        assert_eq!(
            promisor_remote_info(&c).as_deref(),
            Some("name=lop,url=file:///lop,partialCloneFilter=blob:none,token=fooBar")
        );
    }

    #[test]
    fn accept_all() {
        let server = cfg(&[
            ("promisor.advertise", "true"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        let info = promisor_remote_info(&server).unwrap();
        let client = cfg(&[
            ("promisor.acceptFromServer", "All"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        let r = promisor_remote_reply(&client, &info);
        assert_eq!(r.reply.as_deref(), Some("lop"));
    }

    #[test]
    fn accept_none_default() {
        let client = cfg(&[
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        let r = promisor_remote_reply(&client, "name=lop,url=file:///lop");
        assert_eq!(r.reply, None);
    }

    #[test]
    fn known_url_mismatch_rejected() {
        let client = cfg(&[
            ("promisor.acceptFromServer", "KnownUrl"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///other"),
        ]);
        let r = promisor_remote_reply(&client, "name=lop,url=file:///lop");
        assert_eq!(r.reply, None);
    }
}
