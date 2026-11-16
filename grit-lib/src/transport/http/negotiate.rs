//! SPNEGO / Kerberos (`Negotiate`) behind a small GSS context trait.
//!
//! Unix uses system GSSAPI when the `http-negotiate-gss` feature is enabled.
//! Windows compiles an SSPI backend; SSPI is not exercised in this repository's CI.

use crate::error::{AuthFailure, Error, Result};
use crate::remote::RemoteError;

use super::auth::AuthScheme;

/// GSS security context for one SPNEGO exchange.
pub(crate) trait GssContext: Send {
    /// Produce the next outbound token, or `None` when the context is complete.
    ///
    /// `in_token` is the server's previous `WWW-Authenticate` payload when continuing.
    fn step(&mut self, in_token: Option<&[u8]>) -> Result<Option<Vec<u8>>>;

    /// Target HTTP host for the `HTTP@host` GSS service principal.
    fn set_service_host(&mut self, host: &str);
}

/// Begin a new SPNEGO client context for HTTP `Negotiate`.
pub(crate) fn new_negotiate_context() -> Result<Box<dyn GssContext>> {
    #[cfg(all(unix, feature = "http-negotiate-gss"))]
    {
        Ok(Box::new(UnixGssContext::default()))
    }
    #[cfg(all(windows, feature = "http-negotiate"))]
    {
        Ok(Box::new(WindowsSspiContext::default()))
    }
    #[cfg(not(any(
        all(unix, feature = "http-negotiate-gss"),
        all(windows, feature = "http-negotiate"),
    )))]
    {
        Err(mechanism_error(
            AuthScheme::Negotiate,
            "enable feature http-negotiate-gss with system krb5/GSSAPI, or use Windows SSPI"
                .to_owned(),
        ))
    }
}

fn mechanism_error(scheme: AuthScheme, detail: String) -> Error {
    Error::Remote(RemoteError::Auth {
        url: String::new(),
        reason: AuthFailure::MechanismFailed {
            scheme: scheme.name().to_owned(),
            detail,
        },
    })
}

#[cfg(all(unix, feature = "http-negotiate-gss"))]
#[derive(Default)]
struct UnixGssContext {
    host: String,
    inner: Option<libgssapi::context::ClientCtx>,
}

#[cfg(all(unix, feature = "http-negotiate-gss"))]
impl UnixGssContext {
    fn service_name(&self) -> Result<libgssapi::name::Name> {
        let principal = format!("HTTP@{}", self.host);
        libgssapi::name::Name::new(
            principal.as_bytes(),
            Some(&libgssapi::oid::GSS_NT_HOSTBASED_SERVICE),
        )
        .map_err(|e| mechanism_error(AuthScheme::Negotiate, e.to_string()))
    }

    fn ensure_client(&mut self) -> Result<&mut libgssapi::context::ClientCtx> {
        use libgssapi::context::{ClientCtx, CtxFlags};
        use libgssapi::oid::GSS_MECH_SPNEGO;

        if self.inner.is_none() {
            let name = self.service_name()?;
            self.inner = Some(ClientCtx::new(
                None,
                name,
                CtxFlags::GSS_C_MUTUAL_FLAG,
                Some(&GSS_MECH_SPNEGO),
            ));
        }
        self.inner.as_mut().ok_or_else(|| {
            mechanism_error(
                AuthScheme::Negotiate,
                "GSS client context was not initialized".to_owned(),
            )
        })
    }
}

#[cfg(all(unix, feature = "http-negotiate-gss"))]
impl GssContext for UnixGssContext {
    fn set_service_host(&mut self, host: &str) {
        self.host = host.to_owned();
        self.inner = None;
    }

    fn step(&mut self, in_token: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
        if self.host.is_empty() {
            self.host = "localhost".to_owned();
        }
        let ctx = self.ensure_client()?;
        let out = ctx
            .step(in_token, None)
            .map_err(|e| mechanism_error(AuthScheme::Negotiate, e.to_string()))?;
        Ok(out.map(|buf| buf.to_vec()))
    }
}

#[cfg(all(windows, feature = "http-negotiate"))]
#[path = "negotiate_sspi.rs"]
mod negotiate_sspi;
#[cfg(all(windows, feature = "http-negotiate"))]
use negotiate_sspi::WindowsSspiContext;
