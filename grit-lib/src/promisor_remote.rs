//! The `promisor-remote` protocol v2 capability (see `gitprotocol-v2(5)` and
//! `Documentation/config/promisor.adoc`).
//!
//! A server that keeps some objects on other remotes, typically large blobs on a
//! *large-object promisor* (LOP), can tell clients about those remotes. A client that
//! accepts one lets the server leave out objects the LOP can provide; the client then
//! fetches them from the LOP when it needs them.
//!
//! The exchange has three parts, each with a typed model here:
//!
//! - The server builds a [`PromisorAdvertisement`] from its own promisor remotes
//!   ([`PromisorAdvertisement::from_server_config`]) and sends it as
//!   `promisor-remote=<value>` ([`PromisorAdvertisement::to_capability_value`]).
//! - The client parses it ([`PromisorAdvertisement::parse`]), reads its
//!   [`PromisorAcceptPolicy`] (`promisor.acceptFromServer`, `promisor.checkFields`,
//!   `promisor.storeFields`) and runs [`PromisorDecision::decide`] against the promisor
//!   remotes it already has ([`configured_promisor_remotes`]). The names it accepted go
//!   back to the server ([`PromisorDecision::reply_value`]).
//! - The server reads that reply with [`parse_accepted_reply`].
//!
//! Accepting a remote never creates it: only remotes the client already configured can be
//! accepted by name, and lazy fetches always use the locally configured URL, never the
//! advertised one. [`store_advertised_fields`] can update the `partialCloneFilter` and
//! `token` of an accepted, already configured remote.
//!
//! The older string-based helpers ([`promisor_remote_info`], [`promisor_remote_reply`],
//! [`parse_advertisement`]) are thin wrappers over these types.

use std::path::Path;

use crate::config::{parse_bool, ConfigFile, ConfigScope, ConfigSet};
use crate::error::{Error, Result};
use crate::partial_clone::ObjectFilterSpec;

/// An optional per-remote field the `promisor-remote` capability can carry besides
/// `name` and `url`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum PromisorField {
    /// The partial-clone filter the remote is meant to be used with
    /// (`remote.<name>.partialCloneFilter`).
    PartialCloneFilter,
    /// An authentication token for the remote (`remote.<name>.token`).
    Token,
}

impl PromisorField {
    /// Every field, in the order Git writes them in an advertisement.
    pub const ALL: [Self; 2] = [Self::PartialCloneFilter, Self::Token];

    /// The field name as it appears on the wire (case-sensitive).
    ///
    /// # Returns
    ///
    /// `"partialCloneFilter"` or `"token"`.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::PartialCloneFilter => "partialCloneFilter",
            Self::Token => "token",
        }
    }

    /// The canonical (lowercase) last component of the `remote.<name>.<field>` config key.
    ///
    /// # Returns
    ///
    /// `"partialclonefilter"` or `"token"`.
    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::PartialCloneFilter => "partialclonefilter",
            Self::Token => "token",
        }
    }

    /// Parse a comma or whitespace separated field list, as in `promisor.sendFields`.
    ///
    /// Names are matched case-insensitively. Duplicates are kept only once.
    ///
    /// # Parameters
    ///
    /// - `raw`: the config value, for example `"partialCloneFilter, token"`.
    ///
    /// # Returns
    ///
    /// The known fields in the order given, and every name that is not a known field.
    #[must_use]
    pub fn parse_list(raw: &str) -> (Vec<Self>, Vec<String>) {
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for name in raw
            .split([',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match Self::ALL
                .into_iter()
                .find(|f| f.wire_name().eq_ignore_ascii_case(name))
            {
                Some(field) if !known.contains(&field) => known.push(field),
                Some(_) => {}
                None => unknown.push(name.to_owned()),
            }
        }
        (known, unknown)
    }

    fn value_of(self, info: &PromisorInfo) -> Option<&str> {
        match self {
            Self::PartialCloneFilter => info.filter.as_deref(),
            Self::Token => info.token.as_deref(),
        }
    }

    fn set_on(self, info: &mut PromisorInfo, value: String) {
        match self {
            Self::PartialCloneFilter => info.filter = Some(value),
            Self::Token => info.token = Some(value),
        }
    }
}

/// Which advertised promisor remotes a client accepts (`promisor.acceptFromServer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum AcceptFromServer {
    /// Accept nothing (the default).
    #[default]
    None,
    /// Accept a remote configured locally under the same name with the same URL.
    KnownUrl,
    /// Accept a remote configured locally under the same name, whatever its URL.
    KnownName,
    /// Accept every advertised remote. Only for fully trusted servers.
    All,
}

impl AcceptFromServer {
    /// Parse a `promisor.acceptFromServer` value (case-insensitive).
    ///
    /// # Returns
    ///
    /// `None` when `value` is not one of `none`, `knownUrl`, `knownName` or `all`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        [
            ("none", Self::None),
            ("knownurl", Self::KnownUrl),
            ("knownname", Self::KnownName),
            ("all", Self::All),
        ]
        .into_iter()
        .find_map(|(name, policy)| value.eq_ignore_ascii_case(name).then_some(policy))
    }
}

/// A promisor-related config value the library ignored.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum PromisorConfigWarning {
    /// `promisor.acceptFromServer` has a value that is not a known policy; nothing is accepted.
    UnknownAcceptValue(String),
    /// A field list (`promisor.checkFields`, `promisor.storeFields`) names an unknown field.
    UnknownField {
        /// The config key holding the list.
        key: &'static str,
        /// The unknown field name.
        field: String,
    },
    /// An advertised field value was not stored because it is not valid: a filter that does
    /// not parse, or a token containing control characters.
    InvalidAdvertisedValue {
        /// The remote the value was advertised for.
        remote: String,
        /// Which field.
        field: PromisorField,
    },
}

/// The client's rules for accepting advertised promisor remotes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct PromisorAcceptPolicy {
    /// `promisor.acceptFromServer`.
    pub accept: AcceptFromServer,
    /// `promisor.checkFields`: fields whose advertised value must equal the local one.
    pub check_fields: Vec<PromisorField>,
    /// `promisor.storeFields`: fields copied from the advertisement into local config.
    pub store_fields: Vec<PromisorField>,
}

impl PromisorAcceptPolicy {
    /// Read the policy from config.
    ///
    /// # Parameters
    ///
    /// - `cfg`: the client's merged config (local, global, `-c` overrides).
    ///
    /// # Returns
    ///
    /// The policy and any values that were ignored. An unknown `acceptFromServer` value
    /// means [`AcceptFromServer::None`].
    #[must_use]
    pub fn from_config(cfg: &ConfigSet) -> (Self, Vec<PromisorConfigWarning>) {
        let mut warnings = Vec::new();
        let accept = match cfg.get("promisor.acceptFromServer") {
            None => AcceptFromServer::None,
            Some(raw) => AcceptFromServer::parse(&raw).unwrap_or_else(|| {
                warnings.push(PromisorConfigWarning::UnknownAcceptValue(raw));
                AcceptFromServer::None
            }),
        };
        let mut fields = |key: &'static str| {
            let (known, unknown) = PromisorField::parse_list(&cfg.get(key).unwrap_or_default());
            warnings.extend(
                unknown
                    .into_iter()
                    .map(|field| PromisorConfigWarning::UnknownField { key, field }),
            );
            known
        };
        let check_fields = fields("promisor.checkFields");
        let store_fields = fields("promisor.storeFields");
        (
            Self {
                accept,
                check_fields,
                store_fields,
            },
            warnings,
        )
    }
}

/// One advertised or configured promisor remote.
///
/// `Debug` output never shows the token.
#[derive(Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct PromisorInfo {
    /// Remote name.
    pub name: String,
    /// Remote URL.
    pub url: Option<String>,
    /// Partial-clone filter (`partialCloneFilter` field).
    pub filter: Option<String>,
    /// Authentication token (`token` field). Never serialized.
    #[cfg_attr(feature = "serde-models", serde(skip))]
    pub token: Option<String>,
}

impl std::fmt::Debug for PromisorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromisorInfo")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("filter", &self.filter)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The promisor remotes a server advertises with the `promisor-remote` capability.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct PromisorAdvertisement {
    /// Advertised remotes in advertisement order. Each has a name and a non-empty URL.
    pub remotes: Vec<PromisorInfo>,
}

impl PromisorAdvertisement {
    /// Build the advertisement a server sends.
    ///
    /// Only promisor remotes with a non-empty URL are advertised; the fields listed in
    /// `promisor.sendFields` are included when set to a non-empty value.
    ///
    /// # Parameters
    ///
    /// - `cfg`: the served repository's config.
    ///
    /// # Returns
    ///
    /// `None` when `promisor.advertise` is not true or there is no remote to advertise.
    #[must_use]
    pub fn from_server_config(cfg: &ConfigSet) -> Option<Self> {
        let advertise = cfg
            .get_bool("promisor.advertise")
            .and_then(|r| r.ok())
            .unwrap_or(false);
        if !advertise {
            return None;
        }
        let (fields, _unknown) =
            PromisorField::parse_list(&cfg.get("promisor.sendFields").unwrap_or_default());
        let remotes = configured_promisor_remotes(cfg, &fields);
        (!remotes.is_empty()).then_some(Self { remotes })
    }

    /// Parse the value of a server's `promisor-remote=` capability.
    ///
    /// Remotes are separated by `;` and fields by `,`; values are percent-decoded. An entry
    /// without a name or with a missing or empty URL is dropped, and unknown fields are
    /// ignored, as the protocol requires.
    ///
    /// # Parameters
    ///
    /// - `value`: the text after `promisor-remote=`.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        Self {
            remotes: value.split(';').filter_map(parse_one_advertised).collect(),
        }
    }

    /// Serialize as the value of the `promisor-remote=` capability.
    ///
    /// # Returns
    ///
    /// `name=<n>,url=<u>[,partialCloneFilter=<f>][,token=<t>]` per remote, joined with `;`.
    #[must_use]
    pub fn to_capability_value(&self) -> String {
        let mut out = String::new();
        for (i, p) in self.remotes.iter().enumerate() {
            if i != 0 {
                out.push(';');
            }
            out.push_str("name=");
            out.push_str(&urlencode(&p.name));
            out.push_str(",url=");
            out.push_str(&urlencode(p.url.as_deref().unwrap_or("")));
            for field in PromisorField::ALL {
                if let Some(v) = field.value_of(p) {
                    out.push(',');
                    out.push_str(field.wire_name());
                    out.push('=');
                    out.push_str(&urlencode(v));
                }
            }
        }
        out
    }

    /// Names of the advertised remotes, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.remotes.iter().map(|r| r.name.as_str())
    }
}

/// URL-encode a field value: only `,`, `;`, `%` and bytes outside the printable ASCII range
/// are escaped, which is what the capability grammar requires.
fn urlencode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b != b',' && b != b';' && b != b'%' && (33..=126).contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Percent-decode a field value. Malformed escapes are kept as they are.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| char::from(b).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
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

/// Parse one `name=…,url=…` element. `None` without a name or a non-empty URL.
fn parse_one_advertised(remote_info: &str) -> Option<PromisorInfo> {
    let mut info = PromisorInfo::default();
    for elem in remote_info.split(',') {
        let Some((field, value)) = elem.split_once('=') else {
            continue;
        };
        let value = urldecode(value);
        match field {
            "name" => info.name = value,
            "url" => info.url = Some(value),
            other => {
                if let Some(f) = PromisorField::ALL
                    .into_iter()
                    .find(|f| f.wire_name() == other)
                {
                    f.set_on(&mut info, value);
                }
            }
        }
    }
    if info.name.is_empty() || info.url.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    Some(info)
}

/// Names of the promisor remotes in `cfg`, in first-seen config order, with the
/// `extensions.partialClone` remote moved to the end.
///
/// A remote is a promisor remote when `remote.<name>.promisor` is true or
/// `remote.<name>.partialCloneFilter` is set, or when it is named by `extensions.partialClone`.
fn promisor_remote_names(cfg: &ConfigSet) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for e in cfg.entries() {
        let Some(rest) = e.key.strip_prefix("remote.") else {
            continue;
        };
        let name = if let Some(name) = rest.strip_suffix(".promisor") {
            let on = e.value.as_deref().map_or(Ok(true), parse_bool);
            on.unwrap_or(false).then_some(name)
        } else {
            rest.strip_suffix(".partialclonefilter")
        };
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        }
    }
    if let Some(pc) = cfg.get("extensions.partialclone").filter(|s| !s.is_empty()) {
        names.retain(|n| *n != pc);
        names.push(pc);
    }
    names
}

/// The promisor remotes configured in `cfg` that have a non-empty URL.
///
/// Remotes come in first-seen config order with the `extensions.partialClone` remote last,
/// which is the order Git advertises them and tries them for lazy fetches.
///
/// # Parameters
///
/// - `cfg`: merged config.
/// - `fields`: optional fields to read for each remote (`remote.<name>.<field>`); unset or
///   empty values are left `None`.
#[must_use]
pub fn configured_promisor_remotes(cfg: &ConfigSet, fields: &[PromisorField]) -> Vec<PromisorInfo> {
    promisor_remote_names(cfg)
        .into_iter()
        .filter_map(|name| {
            let url = cfg
                .get(&format!("remote.{name}.url"))
                .filter(|u| !u.is_empty())?;
            let mut info = PromisorInfo {
                name,
                url: Some(url),
                ..Default::default()
            };
            for &field in fields {
                if let Some(v) = cfg
                    .get(&format!("remote.{}.{}", info.name, field.config_key()))
                    .filter(|v| !v.is_empty())
                {
                    field.set_on(&mut info, v);
                }
            }
            Some(info)
        })
        .collect()
}

/// The order in which to try promisor remotes for a lazy fetch.
///
/// Names in `preferred` that are still configured promisor remotes come first, then the rest
/// in config order, with the `extensions.partialClone` remote last.
///
/// # Parameters
///
/// - `cfg`: merged config.
/// - `preferred`: remotes to try first, such as the ones a server just had the client accept.
#[must_use]
pub fn lazy_fetch_order(cfg: &ConfigSet, preferred: &[String]) -> Vec<String> {
    let all: Vec<String> = configured_promisor_remotes(cfg, &[])
        .into_iter()
        .map(|p| p.name)
        .collect();
    let mut order: Vec<String> =
        preferred
            .iter()
            .filter(|p| all.contains(p))
            .fold(Vec::new(), |mut acc, p| {
                if !acc.contains(p) {
                    acc.push(p.clone());
                }
                acc
            });
    for name in all {
        if !order.contains(&name) {
            order.push(name);
        }
    }
    order
}

/// Why an advertised promisor remote was not accepted.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum PromisorRejection {
    /// `promisor.acceptFromServer` is `none` (or unset).
    PolicyNone,
    /// No local promisor remote has the advertised name.
    UnknownRemote,
    /// A local remote has the name but a different URL (`knownUrl` policy).
    UrlMismatch {
        /// The locally configured URL.
        configured: String,
    },
    /// A `promisor.checkFields` field was not advertised or is not set locally.
    FieldMissing(PromisorField),
    /// A `promisor.checkFields` field differs between the advertisement and local config.
    FieldMismatch(PromisorField),
    /// The fetch is not a partial fetch, so the client cannot do without the objects the
    /// server would leave out.
    NotPartialFetch,
}

/// One advertised remote and the client's verdict on it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct PromisorCandidate {
    /// The remote as the server advertised it.
    pub advertised: PromisorInfo,
    /// `None` when accepted, otherwise why not.
    pub rejection: Option<PromisorRejection>,
}

impl PromisorCandidate {
    /// Whether the client accepted this remote.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        self.rejection.is_none()
    }
}

/// The client's verdict on every advertised promisor remote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct PromisorDecision {
    /// Every advertised remote, in advertisement order.
    pub candidates: Vec<PromisorCandidate>,
}

impl PromisorDecision {
    /// Decide which advertised remotes to accept.
    ///
    /// Follows `promisor.acceptFromServer`: `knownUrl` needs a local promisor remote with the
    /// same name and the same URL, `knownName` the same name, and `all` accepts any remote.
    /// Accepted remotes must then pass `promisor.checkFields`: each checked field must be
    /// advertised, set locally, and equal. Under `all`, a remote not known locally passes a
    /// field check when any local promisor remote has that value. Comparisons are
    /// case-sensitive.
    ///
    /// # Parameters
    ///
    /// - `policy`: the client's policy.
    /// - `local`: the client's promisor remotes, with the checked fields filled
    ///   ([`configured_promisor_remotes`] with `policy.check_fields`).
    /// - `adv`: what the server advertised.
    #[must_use]
    pub fn decide(
        policy: &PromisorAcceptPolicy,
        local: &[PromisorInfo],
        adv: &PromisorAdvertisement,
    ) -> Self {
        let candidates = adv
            .remotes
            .iter()
            .map(|advertised| PromisorCandidate {
                advertised: advertised.clone(),
                rejection: rejection_for(policy, local, advertised),
            })
            .collect();
        Self { candidates }
    }

    /// The accepted remotes, in advertisement order.
    pub fn accepted(&self) -> impl Iterator<Item = &PromisorInfo> {
        self.candidates
            .iter()
            .filter(|c| c.is_accepted())
            .map(|c| &c.advertised)
    }

    /// Names of the accepted remotes, in advertisement order.
    #[must_use]
    pub fn accepted_names(&self) -> Vec<String> {
        self.accepted().map(|p| p.name.clone()).collect()
    }

    /// Whether any remote was accepted.
    #[must_use]
    pub fn any_accepted(&self) -> bool {
        self.accepted().next().is_some()
    }

    /// The value of the client's `promisor-remote=` reply.
    ///
    /// # Returns
    ///
    /// The URL-encoded accepted names joined with `;`, or `None` when nothing was accepted
    /// (then no reply is sent).
    #[must_use]
    pub fn reply_value(&self) -> Option<String> {
        let names: Vec<String> = self.accepted().map(|p| urlencode(&p.name)).collect();
        (!names.is_empty()).then(|| names.join(";"))
    }

    /// Reject every remote that is still accepted, for `why`.
    pub fn decline_all(&mut self, why: PromisorRejection) {
        for c in &mut self.candidates {
            if c.rejection.is_none() {
                c.rejection = Some(why.clone());
            }
        }
    }

    /// The partial-clone filter implied by the accepted remotes' advertised filters, for a
    /// fetch that asked to pick its filter automatically (Git's `--filter=auto`).
    ///
    /// `blob:none` wins over any limit; otherwise the smallest `blob:limit` is used, which
    /// leaves out every blob any accepted remote expects to hold.
    ///
    /// # Returns
    ///
    /// The filter (`None` when no accepted remote advertised a usable one) and the
    /// advertised filters that could not be used because they are not `blob:none` or
    /// `blob:limit=<n>`.
    #[must_use]
    pub fn combined_filter(&self) -> (Option<ObjectFilterSpec>, Vec<String>) {
        let mut best: Option<ObjectFilterSpec> = None;
        let mut unsupported = Vec::new();
        for spec in self.accepted().filter_map(|p| p.filter.as_deref()) {
            let Ok(parsed) = ObjectFilterSpec::parse(spec) else {
                unsupported.push(spec.to_owned());
                continue;
            };
            best = Some(match (best, parsed) {
                (None, p) => p,
                (Some(ObjectFilterSpec::BlobNone), _) | (_, ObjectFilterSpec::BlobNone) => {
                    ObjectFilterSpec::BlobNone
                }
                (Some(ObjectFilterSpec::BlobLimit(a)), ObjectFilterSpec::BlobLimit(b)) => {
                    ObjectFilterSpec::BlobLimit(a.min(b))
                }
            });
        }
        (best, unsupported)
    }
}

fn rejection_for(
    policy: &PromisorAcceptPolicy,
    local: &[PromisorInfo],
    advertised: &PromisorInfo,
) -> Option<PromisorRejection> {
    let known = local.iter().find(|p| p.name == advertised.name);
    match (policy.accept, known) {
        (AcceptFromServer::None, _) => Some(PromisorRejection::PolicyNone),
        (AcceptFromServer::All, _) => fields_rejection(policy, local, advertised, None),
        (_, None) => Some(PromisorRejection::UnknownRemote),
        (AcceptFromServer::KnownName, Some(p)) => {
            fields_rejection(policy, local, advertised, Some(p))
        }
        (AcceptFromServer::KnownUrl, Some(p)) => {
            if p.url != advertised.url {
                return Some(PromisorRejection::UrlMismatch {
                    configured: p.url.clone().unwrap_or_default(),
                });
            }
            fields_rejection(policy, local, advertised, Some(p))
        }
    }
}

/// Check `promisor.checkFields` for one advertised remote, against `entry` when the remote is
/// known locally, otherwise against any local promisor remote.
fn fields_rejection(
    policy: &PromisorAcceptPolicy,
    local: &[PromisorInfo],
    advertised: &PromisorInfo,
    entry: Option<&PromisorInfo>,
) -> Option<PromisorRejection> {
    for &field in &policy.check_fields {
        let Some(value) = field.value_of(advertised) else {
            return Some(PromisorRejection::FieldMissing(field));
        };
        let matches = match entry {
            Some(p) => match field.value_of(p) {
                None => return Some(PromisorRejection::FieldMissing(field)),
                Some(local_value) => local_value == value,
            },
            None => local.iter().any(|p| field.value_of(p) == Some(value)),
        };
        if !matches {
            return Some(PromisorRejection::FieldMismatch(field));
        }
    }
    None
}

/// A field [`store_advertised_fields`] wrote into local config.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct StoredPromisorField {
    /// The remote whose config changed.
    pub remote: String,
    /// The field written to `remote.<remote>.<field>`.
    pub field: PromisorField,
    /// Whether a different value was there before (otherwise the key was unset).
    pub replaced: bool,
}

/// Store the advertised values of `promisor.storeFields` for accepted remotes.
///
/// Only remotes already configured locally as promisor remotes are touched; nothing is
/// created and no URL is ever written. A filter that does not parse and a token containing
/// control characters are skipped with a warning. Unchanged values are left alone.
///
/// # Parameters
///
/// - `git_dir`: the repository whose local `config` file is updated.
/// - `policy`: the client's policy; only `store_fields` is used.
/// - `cfg`: the client's merged config, for the current values.
/// - `decision`: the decision made for this fetch.
///
/// # Returns
///
/// The fields written, and the values skipped as invalid.
///
/// # Errors
///
/// Returns [`Error`] when the local config file cannot be read or written.
pub fn store_advertised_fields(
    git_dir: &Path,
    policy: &PromisorAcceptPolicy,
    cfg: &ConfigSet,
    decision: &PromisorDecision,
) -> Result<(Vec<StoredPromisorField>, Vec<PromisorConfigWarning>)> {
    let mut stored = Vec::new();
    let mut warnings = Vec::new();
    if policy.store_fields.is_empty() {
        return Ok((stored, warnings));
    }
    let local = configured_promisor_remotes(cfg, &policy.store_fields);
    let mut writes: Vec<(String, String)> = Vec::new();
    for advertised in decision.accepted() {
        let Some(known) = local.iter().find(|p| p.name == advertised.name) else {
            continue;
        };
        for &field in &policy.store_fields {
            let Some(value) = field.value_of(advertised) else {
                continue;
            };
            let current = field.value_of(known);
            if current == Some(value) {
                continue;
            }
            let valid = match field {
                PromisorField::PartialCloneFilter => {
                    crate::rev_list::ObjectFilter::parse(value).is_ok()
                }
                PromisorField::Token => !value.chars().any(char::is_control),
            };
            if !valid {
                warnings.push(PromisorConfigWarning::InvalidAdvertisedValue {
                    remote: advertised.name.clone(),
                    field,
                });
                continue;
            }
            writes.push((
                format!("remote.{}.{}", advertised.name, field.config_key()),
                value.to_owned(),
            ));
            stored.push(StoredPromisorField {
                remote: advertised.name.clone(),
                field,
                replaced: current.is_some(),
            });
        }
    }
    if writes.is_empty() {
        return Ok((stored, warnings));
    }
    let path = git_dir.join("config");
    let content = std::fs::read_to_string(&path).map_err(Error::Io)?;
    let mut file = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
    for (key, value) in &writes {
        file.set(key, value)?;
    }
    file.write()?;
    Ok((stored, warnings))
}

/// A malformed client `promisor-remote=` reply.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromisorReplyError {
    /// The reply carried no remote names.
    #[error("empty promisor-remote reply")]
    Empty,
}

/// Server side: parse the value of a client's `promisor-remote=` reply into remote names.
///
/// # Parameters
///
/// - `value`: the text after `promisor-remote=`; `;`-separated, URL-encoded names.
///
/// # Errors
///
/// [`PromisorReplyError::Empty`] when no name is given.
pub fn parse_accepted_reply(value: &str) -> std::result::Result<Vec<String>, PromisorReplyError> {
    let mut names: Vec<String> = Vec::new();
    for name in value.split(';').map(urldecode).filter(|n| !n.is_empty()) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return Err(PromisorReplyError::Empty);
    }
    Ok(names)
}

/// Build the server's `promisor-remote=<info>` advertisement value.
///
/// Same as [`PromisorAdvertisement::from_server_config`] followed by
/// [`PromisorAdvertisement::to_capability_value`].
///
/// # Returns
///
/// `None` when `promisor.advertise` is not true or there are no advertisable promisor remotes.
#[must_use]
pub fn promisor_remote_info(cfg: &ConfigSet) -> Option<String> {
    PromisorAdvertisement::from_server_config(cfg).map(|a| a.to_capability_value())
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
///
/// Same as [`PromisorAdvertisement::parse`].
#[must_use]
pub fn parse_advertisement(info: &str) -> Vec<PromisorInfo> {
    PromisorAdvertisement::parse(info).remotes
}

/// Client side: decide which advertised promisor remotes to accept and build the reply.
///
/// A string-based wrapper over [`PromisorAcceptPolicy::from_config`] and
/// [`PromisorDecision::decide`].
///
/// # Parameters
///
/// - `cfg`: the client's config.
/// - `info`: the raw value of the server's `promisor-remote=` capability.
#[must_use]
pub fn promisor_remote_reply(cfg: &ConfigSet, info: &str) -> PromisorReply {
    let (policy, _warnings) = PromisorAcceptPolicy::from_config(cfg);
    let local = configured_promisor_remotes(cfg, &policy.check_fields);
    let decision = PromisorDecision::decide(&policy, &local, &PromisorAdvertisement::parse(info));
    PromisorReply {
        reply: decision.reply_value(),
        accepted: decision.accepted_names(),
        accepted_filters: decision
            .accepted()
            .filter_map(|p| p.filter.clone().map(|f| (p.name.clone(), f)))
            .collect(),
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

    fn adv(value: &str) -> PromisorAdvertisement {
        PromisorAdvertisement::parse(value)
    }

    fn decide(client: &[(&str, &str)], value: &str) -> PromisorDecision {
        let c = cfg(client);
        let (policy, _) = PromisorAcceptPolicy::from_config(&c);
        let local = configured_promisor_remotes(&c, &policy.check_fields);
        PromisorDecision::decide(&policy, &local, &adv(value))
    }

    fn rejection(d: &PromisorDecision) -> Option<PromisorRejection> {
        d.candidates[0].rejection.clone()
    }

    #[test]
    fn no_advertise_when_disabled() {
        let c = cfg(&[
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        assert_eq!(promisor_remote_info(&c), None);
        let c = cfg(&[
            ("promisor.advertise", "false"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        assert_eq!(PromisorAdvertisement::from_server_config(&c), None);
    }

    #[test]
    fn no_advertise_without_promisor_remotes() {
        let c = cfg(&[
            ("promisor.advertise", "true"),
            ("remote.origin.url", "file:///o"),
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
    fn advertise_escapes_separators_percent_and_space() {
        let c = cfg(&[
            ("promisor.advertise", "true"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///a b,c;d%e"),
        ]);
        assert_eq!(
            promisor_remote_info(&c).as_deref(),
            Some("name=lop,url=file:///a%20b%2Cc%3Bd%25e")
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
    fn advertise_skips_empty_url_and_lists_partial_clone_remote_last() {
        let c = cfg(&[
            ("promisor.advertise", "true"),
            ("extensions.partialClone", "origin"),
            ("remote.origin.promisor", "true"),
            ("remote.origin.url", "file:///origin"),
            ("remote.empty.promisor", "true"),
            ("remote.empty.url", ""),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
        ]);
        assert_eq!(
            promisor_remote_info(&c).as_deref(),
            Some("name=lop,url=file:///lop;name=origin,url=file:///origin")
        );
    }

    #[test]
    fn partial_clone_extension_alone_makes_a_promisor_remote() {
        let c = cfg(&[
            ("extensions.partialClone", "origin"),
            ("remote.origin.url", "file:///origin"),
            ("remote.other.promisor", "false"),
            ("remote.other.url", "file:///other"),
        ]);
        let names: Vec<String> = configured_promisor_remotes(&c, &[])
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["origin"]);
    }

    #[test]
    fn parse_ignores_unknown_fields_and_drops_bad_entries() {
        let a = adv(
            "name=lop,url=file:///l%20p,foo=bar,partialCloneFilter=blob:limit%3D5k;\
             name=nourl;name=empty,url=;url=file:///noname",
        );
        assert_eq!(a.remotes.len(), 1);
        assert_eq!(a.remotes[0].name, "lop");
        assert_eq!(a.remotes[0].url.as_deref(), Some("file:///l p"));
        assert_eq!(a.remotes[0].filter.as_deref(), Some("blob:limit=5k"));
        assert_eq!(parse_advertisement("name=x").len(), 0);
    }

    #[test]
    fn parse_round_trips_capability_value() {
        let original = PromisorAdvertisement {
            remotes: vec![
                PromisorInfo {
                    name: "a;b".into(),
                    url: Some("https://h/p,q".into()),
                    filter: Some("blob:none".into()),
                    token: Some("t%k".into()),
                },
                PromisorInfo {
                    name: "c".into(),
                    url: Some("file:///c".into()),
                    ..Default::default()
                },
            ],
        };
        let value = original.to_capability_value();
        assert_eq!(PromisorAdvertisement::parse(&value), original);
        assert_eq!(original.names().collect::<Vec<_>>(), ["a;b", "c"]);
    }

    #[test]
    fn field_list_parsing() {
        let (known, unknown) = PromisorField::parse_list("PartialCloneFilter token,foo token");
        assert_eq!(
            known,
            [PromisorField::PartialCloneFilter, PromisorField::Token]
        );
        assert_eq!(unknown, ["foo"]);
        assert_eq!(PromisorField::Token.config_key(), "token");
        assert_eq!(
            PromisorField::PartialCloneFilter.config_key(),
            "partialclonefilter"
        );
    }

    #[test]
    fn policy_values_are_case_insensitive_and_unknown_warns() {
        for (raw, want) in [
            ("ALL", AcceptFromServer::All),
            ("knownurl", AcceptFromServer::KnownUrl),
            ("KnownName", AcceptFromServer::KnownName),
            ("None", AcceptFromServer::None),
        ] {
            let (p, w) =
                PromisorAcceptPolicy::from_config(&cfg(&[("promisor.acceptFromServer", raw)]));
            assert_eq!(p.accept, want);
            assert!(w.is_empty());
        }
        let (p, w) = PromisorAcceptPolicy::from_config(&cfg(&[
            ("promisor.acceptFromServer", "sometimes"),
            ("promisor.checkFields", "bogus"),
        ]));
        assert_eq!(p.accept, AcceptFromServer::None);
        assert_eq!(
            w,
            [
                PromisorConfigWarning::UnknownAcceptValue("sometimes".into()),
                PromisorConfigWarning::UnknownField {
                    key: "promisor.checkFields",
                    field: "bogus".into()
                }
            ]
        );
    }

    const LOP: [(&str, &str); 2] = [
        ("remote.lop.promisor", "true"),
        ("remote.lop.url", "file:///lop"),
    ];

    #[test]
    fn accept_none_by_default() {
        let d = decide(&LOP, "name=lop,url=file:///lop");
        assert_eq!(rejection(&d), Some(PromisorRejection::PolicyNone));
        assert_eq!(d.reply_value(), None);
        assert!(!d.any_accepted());
    }

    #[test]
    fn known_url_needs_matching_url() {
        let mut c = LOP.to_vec();
        c.push(("promisor.acceptFromServer", "KnownUrl"));
        let d = decide(&c, "name=lop,url=file:///lop");
        assert_eq!(d.reply_value().as_deref(), Some("lop"));
        let d = decide(&c, "name=lop,url=file:///elsewhere");
        assert_eq!(
            rejection(&d),
            Some(PromisorRejection::UrlMismatch {
                configured: "file:///lop".into()
            })
        );
        let d = decide(&c, "name=other,url=file:///lop");
        assert_eq!(rejection(&d), Some(PromisorRejection::UnknownRemote));
    }

    #[test]
    fn known_name_ignores_url() {
        let mut c = LOP.to_vec();
        c.push(("promisor.acceptFromServer", "knownName"));
        let d = decide(&c, "name=lop,url=file:///elsewhere");
        assert!(d.any_accepted());
        let d = decide(&c, "name=other,url=file:///lop");
        assert_eq!(rejection(&d), Some(PromisorRejection::UnknownRemote));
    }

    #[test]
    fn all_accepts_unknown_remotes() {
        let d = decide(
            &[("promisor.acceptFromServer", "all")],
            "name=lop,url=file:///lop;name=x,url=file:///x",
        );
        assert_eq!(d.accepted_names(), ["lop", "x"]);
        assert_eq!(d.reply_value().as_deref(), Some("lop;x"));
    }

    #[test]
    fn check_fields_conditions() {
        let mut c = LOP.to_vec();
        c.push(("promisor.acceptFromServer", "knownName"));
        c.push(("promisor.checkFields", "partialCloneFilter"));
        // Not set locally.
        let d = decide(&c, "name=lop,url=file:///lop,partialCloneFilter=blob:none");
        assert_eq!(
            rejection(&d),
            Some(PromisorRejection::FieldMissing(
                PromisorField::PartialCloneFilter
            ))
        );
        c.push(("remote.lop.partialCloneFilter", "blob:none"));
        // Not advertised.
        let d = decide(&c, "name=lop,url=file:///lop");
        assert_eq!(
            rejection(&d),
            Some(PromisorRejection::FieldMissing(
                PromisorField::PartialCloneFilter
            ))
        );
        // Different (case-sensitive).
        let d = decide(&c, "name=lop,url=file:///lop,partialCloneFilter=BLOB:none");
        assert_eq!(
            rejection(&d),
            Some(PromisorRejection::FieldMismatch(
                PromisorField::PartialCloneFilter
            ))
        );
        let d = decide(&c, "name=lop,url=file:///lop,partialCloneFilter=blob:none");
        assert!(d.any_accepted());
    }

    #[test]
    fn check_fields_under_all_match_any_local_remote() {
        let c = [
            ("promisor.acceptFromServer", "all"),
            ("promisor.checkFields", "token"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
            ("remote.lop.token", "secret"),
        ];
        assert!(decide(&c, "name=new,url=file:///n,token=secret").any_accepted());
        assert_eq!(
            rejection(&decide(&c, "name=new,url=file:///n,token=other")),
            Some(PromisorRejection::FieldMismatch(PromisorField::Token))
        );
    }

    #[test]
    fn reply_value_encodes_names() {
        let d = decide(
            &[("promisor.acceptFromServer", "all")],
            "name=a%3Bb,url=file:///x",
        );
        assert_eq!(d.reply_value().as_deref(), Some("a%3Bb"));
        assert_eq!(parse_accepted_reply("a%3Bb").unwrap(), ["a;b"]);
    }

    #[test]
    fn decline_all_rejects_accepted_only() {
        let mut c = LOP.to_vec();
        c.push(("promisor.acceptFromServer", "knownName"));
        let mut d = decide(&c, "name=lop,url=file:///lop;name=x,url=file:///x");
        d.decline_all(PromisorRejection::NotPartialFetch);
        assert_eq!(
            d.candidates[0].rejection,
            Some(PromisorRejection::NotPartialFetch)
        );
        assert_eq!(
            d.candidates[1].rejection,
            Some(PromisorRejection::UnknownRemote)
        );
    }

    #[test]
    fn combined_filter_rules() {
        let all = [("promisor.acceptFromServer", "all")];
        let d = decide(
            &all,
            "name=a,url=u,partialCloneFilter=blob:limit=5k;name=b,url=v,partialCloneFilter=blob:none",
        );
        assert_eq!(
            d.combined_filter(),
            (Some(ObjectFilterSpec::BlobNone), vec![])
        );
        let d = decide(
            &all,
            "name=a,url=u,partialCloneFilter=blob:limit=5k;name=b,url=v,partialCloneFilter=blob:limit=100",
        );
        assert_eq!(
            d.combined_filter(),
            (Some(ObjectFilterSpec::BlobLimit(100)), vec![])
        );
        let d = decide(&all, "name=a,url=u,partialCloneFilter=tree:0;name=b,url=v");
        assert_eq!(d.combined_filter(), (None, vec!["tree:0".to_owned()]));
    }

    #[test]
    fn accepted_reply_parsing() {
        assert_eq!(parse_accepted_reply(""), Err(PromisorReplyError::Empty));
        assert_eq!(parse_accepted_reply(";"), Err(PromisorReplyError::Empty));
        assert_eq!(parse_accepted_reply("a;b;a").unwrap(), ["a", "b"]);
    }

    #[test]
    fn lazy_fetch_order_rules() {
        let c = cfg(&[
            ("extensions.partialClone", "origin"),
            ("remote.origin.promisor", "true"),
            ("remote.origin.url", "file:///o"),
            ("remote.a.promisor", "true"),
            ("remote.a.url", "file:///a"),
            ("remote.b.partialCloneFilter", "blob:none"),
            ("remote.b.url", "file:///b"),
        ]);
        assert_eq!(lazy_fetch_order(&c, &[]), ["a", "b", "origin"]);
        assert_eq!(
            lazy_fetch_order(&c, &["b".into(), "gone".into(), "b".into()]),
            ["b", "a", "origin"]
        );
    }

    #[test]
    fn debug_redacts_token() {
        let p = PromisorInfo {
            name: "lop".into(),
            url: Some("u".into()),
            filter: None,
            token: Some("hunter2".into()),
        };
        let shown = format!("{p:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn legacy_reply_wrapper() {
        let server = cfg(&[
            ("promisor.advertise", "true"),
            ("promisor.sendFields", "partialCloneFilter"),
            ("remote.lop.promisor", "true"),
            ("remote.lop.url", "file:///lop"),
            ("remote.lop.partialCloneFilter", "blob:limit=5k"),
        ]);
        let info = promisor_remote_info(&server).unwrap();
        let mut client = LOP.to_vec();
        client.push(("promisor.acceptFromServer", "All"));
        let r = promisor_remote_reply(&cfg(&client), &info);
        assert_eq!(r.reply.as_deref(), Some("lop"));
        assert_eq!(r.accepted, ["lop"]);
        assert_eq!(
            r.accepted_filters,
            [("lop".to_owned(), "blob:limit=5k".to_owned())]
        );
    }

    fn repo_with_config(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), body).unwrap();
        dir
    }

    fn load(dir: &Path) -> ConfigSet {
        ConfigSet::load_repo_local_only(dir).unwrap()
    }

    #[test]
    fn store_fields_writes_known_remotes_only() {
        let dir = repo_with_config(
            "[remote \"lop\"]\n\turl = file:///lop\n\tpromisor = true\n\ttoken = old\n",
        );
        let c = load(dir.path());
        let policy = PromisorAcceptPolicy {
            accept: AcceptFromServer::All,
            check_fields: vec![],
            store_fields: vec![PromisorField::PartialCloneFilter, PromisorField::Token],
        };
        let d = PromisorDecision::decide(
            &policy,
            &configured_promisor_remotes(&c, &[]),
            &adv(
                "name=lop,url=file:///evil,partialCloneFilter=blob:limit=5k,token=new;\
                  name=unknown,url=file:///u,token=t",
            ),
        );
        let (stored, warnings) = store_advertised_fields(dir.path(), &policy, &c, &d).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            stored,
            [
                StoredPromisorField {
                    remote: "lop".into(),
                    field: PromisorField::PartialCloneFilter,
                    replaced: false
                },
                StoredPromisorField {
                    remote: "lop".into(),
                    field: PromisorField::Token,
                    replaced: true
                }
            ]
        );
        let after = load(dir.path());
        assert_eq!(
            after.get("remote.lop.partialclonefilter").as_deref(),
            Some("blob:limit=5k")
        );
        assert_eq!(after.get("remote.lop.token").as_deref(), Some("new"));
        assert_eq!(after.get("remote.lop.url").as_deref(), Some("file:///lop"));
        assert_eq!(after.get("remote.unknown.url"), None);

        // Unchanged values are a no-op.
        let (stored, _) = store_advertised_fields(dir.path(), &policy, &after, &d).unwrap();
        assert!(stored.is_empty());
    }

    #[test]
    fn store_fields_rejects_invalid_values() {
        let dir = repo_with_config("[remote \"lop\"]\n\turl = file:///lop\n\tpromisor = true\n");
        let c = load(dir.path());
        let policy = PromisorAcceptPolicy {
            accept: AcceptFromServer::KnownName,
            check_fields: vec![],
            store_fields: vec![PromisorField::PartialCloneFilter, PromisorField::Token],
        };
        let d = PromisorDecision::decide(
            &policy,
            &configured_promisor_remotes(&c, &[]),
            &adv("name=lop,url=file:///lop,partialCloneFilter=bogus:filter,token=a%0Ab"),
        );
        let (stored, warnings) = store_advertised_fields(dir.path(), &policy, &c, &d).unwrap();
        assert!(stored.is_empty());
        assert_eq!(warnings.len(), 2);
        let after = load(dir.path());
        assert_eq!(after.get("remote.lop.token"), None);
        assert_eq!(after.get("remote.lop.partialclonefilter"), None);
    }
}
