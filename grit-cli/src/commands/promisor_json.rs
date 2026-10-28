//! `promisor_remotes` / `stored_fields` output shared by `grit clone` and `grit fetch`.
//!
//! Token values never reach any output: only remote names, redacted URLs, filters and
//! field names are shown.

use grit_lib::objects::promisor_remote::{
    PromisorConfigWarning, PromisorField, PromisorHandshake, PromisorRejection,
};
use schemars::JsonSchema;
use serde::Serialize;

/// One promisor remote the server advertised, and whether this client accepted it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PromisorRemoteJson {
    /// Remote name as the server advertised it.
    pub name: String,
    /// Advertised URL, with any credentials removed.
    pub url: String,
    /// Whether the client accepted the remote, letting the server leave out what it holds.
    pub accepted: bool,
    /// The partial-clone filter the server advertised for the remote.
    pub filter: Option<String>,
    /// Why the remote was not accepted: `policy_none`, `unknown_remote`, `url_mismatch`,
    /// `field_missing`, `field_mismatch` or `not_partial`; `null` when accepted.
    pub rejected: Option<String>,
}

/// A remote field written into local config from the server's advertisement.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StoredFieldJson {
    /// The remote whose config changed.
    pub remote: String,
    /// `partialCloneFilter` or `token`. The value itself is never shown.
    pub field: String,
}

/// Something about the promisor exchange the client ignored or could not use.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PromisorWarningJson {
    /// `unknown_accept_value` (`promisor.acceptFromServer` is not a known policy, so
    /// nothing is accepted), `unknown_field` (a `promisor.checkFields` /
    /// `promisor.storeFields` entry is not a known field), `invalid_advertised_value` (an
    /// advertised field value was not stored because it is invalid) or
    /// `unusable_auto_filter` (an accepted remote's filter cannot be used by
    /// `--blobs auto`).
    pub code: String,
    /// The config key concerned, for `unknown_accept_value` and `unknown_field`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The remote concerned, for `invalid_advertised_value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The field name, for `unknown_field` and `invalid_advertised_value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// The ignored config value, or the unusable filter. Advertised values that were
    /// rejected as invalid are never shown (they may be tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl PromisorWarningJson {
    fn from_warning(w: &PromisorConfigWarning) -> Self {
        let mut out = Self {
            code: "warning".to_owned(),
            key: None,
            remote: None,
            field: None,
            value: None,
        };
        match w {
            PromisorConfigWarning::UnknownAcceptValue(v) => {
                out.code = "unknown_accept_value".to_owned();
                out.key = Some("promisor.acceptFromServer".to_owned());
                out.value = Some(v.clone());
            }
            PromisorConfigWarning::UnknownField { key, field } => {
                out.code = "unknown_field".to_owned();
                out.key = Some((*key).to_owned());
                out.field = Some(field.clone());
            }
            PromisorConfigWarning::InvalidAdvertisedValue { remote, field } => {
                out.code = "invalid_advertised_value".to_owned();
                out.remote = Some(remote.clone());
                out.field = Some(field_name(*field).to_owned());
            }
            PromisorConfigWarning::UnusableAutoFilter(filter) => {
                out.code = "unusable_auto_filter".to_owned();
                out.value = Some(filter.clone());
            }
            _ => {}
        }
        out
    }

    /// One sentence for people.
    #[must_use]
    pub fn message(&self) -> String {
        let key = self.key.as_deref().unwrap_or("");
        let field = self.field.as_deref().unwrap_or("");
        let value = self.value.as_deref().unwrap_or("");
        match self.code.as_str() {
            "unknown_accept_value" => {
                format!("{key} = {value} is not a known policy, so no promisor remote is accepted")
            }
            "unknown_field" => format!("{key} names unknown field {field}; ignored"),
            "invalid_advertised_value" => format!(
                "advertised {field} for {} is invalid and was not stored",
                self.remote.as_deref().unwrap_or("")
            ),
            "unusable_auto_filter" => {
                format!("--blobs auto cannot use the advertised filter {value}; ignored")
            }
            _ => "promisor exchange warning".to_owned(),
        }
    }
}

/// The warnings of `handshake`, for JSON.
#[must_use]
pub fn warnings(handshake: Option<&PromisorHandshake>) -> Vec<PromisorWarningJson> {
    handshake
        .map(|h| {
            h.warnings
                .iter()
                .map(PromisorWarningJson::from_warning)
                .collect()
        })
        .unwrap_or_default()
}

fn rejection_code(r: &PromisorRejection) -> &'static str {
    match r {
        PromisorRejection::PolicyNone => "policy_none",
        PromisorRejection::UnknownRemote => "unknown_remote",
        PromisorRejection::UrlMismatch { .. } => "url_mismatch",
        PromisorRejection::FieldMissing(_) => "field_missing",
        PromisorRejection::FieldMismatch(_) => "field_mismatch",
        PromisorRejection::NotPartialFetch => "not_partial",
        _ => "rejected",
    }
}

fn field_name(f: PromisorField) -> &'static str {
    f.wire_name()
}

/// The advertised remotes of `handshake`, for JSON.
#[must_use]
pub fn promisor_remotes(handshake: Option<&PromisorHandshake>) -> Vec<PromisorRemoteJson> {
    let Some(h) = handshake else {
        return Vec::new();
    };
    h.decision
        .candidates
        .iter()
        .map(|c| PromisorRemoteJson {
            name: c.advertised.name.clone(),
            url: c.advertised.display_url(),
            accepted: c.is_accepted(),
            filter: c.advertised.filter.clone(),
            rejected: c.rejection.as_ref().map(|r| rejection_code(r).to_owned()),
        })
        .collect()
}

/// The fields `handshake` stored, for JSON.
#[must_use]
pub fn stored_fields(handshake: Option<&PromisorHandshake>) -> Vec<StoredFieldJson> {
    handshake
        .map(|h| {
            h.stored
                .iter()
                .map(|s| StoredFieldJson {
                    remote: s.remote.clone(),
                    field: field_name(s.field).to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Human summary lines, shown under a `promisor` label: one per advertised remote, then one
/// per stored field, then one per warning.
#[must_use]
pub fn human_lines(
    remotes: &[PromisorRemoteJson],
    stored: &[StoredFieldJson],
    warnings: &[PromisorWarningJson],
) -> Vec<String> {
    let mut lines: Vec<String> = remotes
        .iter()
        .map(|r| {
            let verdict = match (&r.rejected, r.accepted) {
                (_, true) => "accepted".to_owned(),
                (Some(why), false) => format!("not accepted ({})", why.replace('_', " ")),
                (None, false) => "not accepted".to_owned(),
            };
            match &r.filter {
                Some(f) if r.accepted => format!("{} {verdict} · {f}", r.name),
                _ => format!("{} {verdict}", r.name),
            }
        })
        .collect();
    lines.extend(
        stored
            .iter()
            .map(|s| format!("stored {} for {}", s.field, s.remote)),
    );
    lines.extend(warnings.iter().map(|w| format!("warning: {}", w.message())));
    lines
}

/// Markdown bullets for `warnings`.
#[must_use]
pub fn markdown_warning_lines(warnings: &[PromisorWarningJson]) -> Vec<String> {
    warnings
        .iter()
        .map(|w| format!("- **promisor warning** (`{}`): {}", w.code, w.message()))
        .collect()
}

/// Markdown bullet listing the advertised remotes, or `None` when there were none.
#[must_use]
pub fn markdown_line(remotes: &[PromisorRemoteJson]) -> Option<String> {
    if remotes.is_empty() {
        return None;
    }
    let list = remotes
        .iter()
        .map(|r| {
            let verdict = if r.accepted {
                "accepted".to_owned()
            } else {
                format!(
                    "not accepted: {}",
                    r.rejected.as_deref().unwrap_or("rejected")
                )
            };
            format!("`{}` ({verdict})", r.name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("- **promisor remotes**: {list}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(accepted: bool) -> PromisorRemoteJson {
        PromisorRemoteJson {
            name: "lop".into(),
            url: "https://lop.example".into(),
            accepted,
            filter: Some("blob:limit=5k".into()),
            rejected: (!accepted).then(|| "url_mismatch".into()),
        }
    }

    #[test]
    fn human_lines_name_the_verdict() {
        let stored = [StoredFieldJson {
            remote: "lop".into(),
            field: "partialCloneFilter".into(),
        }];
        assert_eq!(
            human_lines(&[remote(true)], &stored, &[]),
            [
                "lop accepted · blob:limit=5k",
                "stored partialCloneFilter for lop"
            ]
        );
        assert_eq!(
            human_lines(&[remote(false)], &[], &[]),
            ["lop not accepted (url mismatch)"]
        );
    }

    #[test]
    fn warnings_are_shown_without_advertised_values() {
        let all = [
            PromisorConfigWarning::UnknownAcceptValue("knownurl2".into()),
            PromisorConfigWarning::UnknownField {
                key: "promisor.storeFields",
                field: "bogus".into(),
            },
            PromisorConfigWarning::InvalidAdvertisedValue {
                remote: "lop".into(),
                field: PromisorField::Token,
            },
            PromisorConfigWarning::UnusableAutoFilter("tree:0".into()),
        ];
        let json: Vec<PromisorWarningJson> =
            all.iter().map(PromisorWarningJson::from_warning).collect();
        let codes: Vec<&str> = json.iter().map(|w| w.code.as_str()).collect();
        assert_eq!(
            codes,
            [
                "unknown_accept_value",
                "unknown_field",
                "invalid_advertised_value",
                "unusable_auto_filter"
            ]
        );
        assert_eq!(
            human_lines(&[], &[], &json),
            [
                "warning: promisor.acceptFromServer = knownurl2 is not a known policy, so no promisor remote is accepted",
                "warning: promisor.storeFields names unknown field bogus; ignored",
                "warning: advertised token for lop is invalid and was not stored",
                "warning: --blobs auto cannot use the advertised filter tree:0; ignored",
            ]
        );
        assert_eq!(json[2].value, None);
        assert_eq!(
            markdown_warning_lines(&json[3..]),
            ["- **promisor warning** (`unusable_auto_filter`): --blobs auto cannot use the advertised filter tree:0; ignored"]
        );
    }

    #[test]
    fn markdown_line_lists_remotes() {
        assert_eq!(markdown_line(&[]), None);
        assert_eq!(
            markdown_line(&[remote(true)]).as_deref(),
            Some("- **promisor remotes**: `lop` (accepted)")
        );
    }
}
