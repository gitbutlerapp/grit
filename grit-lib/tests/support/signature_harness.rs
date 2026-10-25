//! Test helpers for [`grit_lib::ident::Signature`] in reflog-related APIs.

use grit_lib::ident::Signature;

/// Build a [`Signature`] from `Name <email>` (appends a fixed timestamp for tests).
#[must_use]
pub fn reflog_signature(name_email: &str) -> Signature {
    format!("{name_email} 1 +0000")
        .parse()
        .unwrap_or_else(|_| panic!("reflog_signature: invalid contact {name_email:?}"))
}

/// Parse a full Git header line, or expand `Name <email>` with a fixed timestamp.
#[must_use]
pub fn parse_reflog_signature(line: &str) -> Signature {
    line.parse().unwrap_or_else(|_| reflog_signature(line))
}
