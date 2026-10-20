//! Remote transport handles attached to a daily scenario context.

use std::path::PathBuf;

use crate::network_fixture::NetworkBenchMeta;
use crate::network_http::{GitHttpBackendServer, GritHttpServer};

/// Live remote state for one daily scenario (keeps HTTP servers alive for the run).
pub struct DailyRemoteState {
    pub bare: PathBuf,
    pub meta: NetworkBenchMeta,
    pub remote_url: String,
    pub grit_http: Option<GritHttpServer>,
    pub git_http: Option<GitHttpBackendServer>,
}
