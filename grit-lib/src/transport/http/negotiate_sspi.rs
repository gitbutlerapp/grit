//! Windows SSPI client for HTTP `Negotiate` (compiled on Windows only).

#[cfg(all(windows, feature = "http-negotiate"))]
mod imp {
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::SEC_E_OK;
    use windows_sys::Win32::Security::Authentication::Identity::{
        SEC_WINNT_AUTH_IDENTITY_VERSION, SEC_WINNT_AUTH_IDENTITY_W,
    };
    use windows_sys::Win32::Security::Credentials::{
        AcquireCredentialsHandleW, FreeCredentialsHandle, InitializeSecurityContextW, SecHandle,
        ISC_REQ_CONFIDENTIALITY, ISC_REQ_CONNECTION, ISC_REQ_MUTUAL_AUTH,
        ISC_REQ_REPLAY_DETECT, ISC_REQ_SEQUENCE_DETECT, SECPKG_CRED, SECPKG_CRED_OUTBOUND,
    };
    use windows_sys::Win32::Security::{
        FreeContextBuffer, SecBuffer, SecBufferDesc, SECBUFFER_TOKEN,
    };

    use crate::error::{AuthFailure, Error, Result};
    use crate::remote::RemoteError;

    use super::super::auth::AuthScheme;
    use super::super::negotiate::GssContext;

    pub(crate) struct WindowsSspiContext {
        host: String,
        cred: SecHandle,
        ctx: SecHandle,
        have_cred: bool,
        have_ctx: bool,
    }

    impl Default for WindowsSspiContext {
        fn default() -> Self {
            Self {
                host: String::new(),
                cred: SecHandle {
                    dwLower: 0,
                    dwUpper: 0,
                },
                ctx: SecHandle {
                    dwLower: 0,
                    dwUpper: 0,
                },
                have_cred: false,
                have_ctx: false,
            }
        }
    }

    impl Drop for WindowsSspiContext {
        fn drop(&mut self) {
            if self.have_cred {
                unsafe {
                    FreeCredentialsHandle(&mut self.cred);
                }
            }
        }
    }

    impl GssContext for WindowsSspiContext {
        fn set_service_host(&mut self, host: &str) {
            self.host = host.to_owned();
            self.have_ctx = false;
        }

        fn step(&mut self, in_token: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
            if self.host.is_empty() {
                self.host = "localhost".to_owned();
            }
            if !self.have_cred {
                acquire_cred(&mut self.cred)?;
                self.have_cred = true;
            }
            let target = format!("HTTP/{}", self.host);
            let target_wide: Vec<u16> = target.encode_utf16().chain([0]).collect();
            let mut output = vec![0u8; 16 * 1024];
            let mut out_buf = SecBuffer {
                cbBuffer: output.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: output.as_mut_ptr().cast(),
            };
            let mut out_desc = SecBufferDesc {
                ulVersion: 0,
                cBuffers: 1,
                pBuffers: &mut out_buf,
            };
            let mut in_bundle = in_token.map(InTokenBundle::new);
            let flags = ISC_REQ_CONFIDENTIALITY
                | ISC_REQ_REPLAY_DETECT
                | ISC_REQ_SEQUENCE_DETECT
                | ISC_REQ_CONNECTION
                | ISC_REQ_MUTUAL_AUTH;
            let status = unsafe {
                InitializeSecurityContextW(
                    &mut self.cred,
                    if self.have_ctx {
                        &mut self.ctx
                    } else {
                        null_mut()
                    },
                    target_wide.as_ptr(),
                    flags,
                    0,
                    0,
                    in_bundle
                        .as_mut()
                        .map(|b| &mut b.desc)
                        .unwrap_or(null_mut()),
                    0,
                    &mut self.ctx,
                    &mut out_desc,
                    null_mut(),
                    null_mut(),
                )
            };
            if status != SEC_E_OK && status != windows_sys::Win32::Foundation::SEC_I_CONTINUE_NEEDED
            {
                return Err(mechanism_error(
                    AuthScheme::Negotiate,
                    format!("InitializeSecurityContext failed with status {status}"),
                ));
            }
            self.have_ctx = true;
            let len = out_buf.cbBuffer as usize;
            output.truncate(len);
            if output.is_empty() {
                Ok(None)
            } else {
                Ok(Some(output))
            }
        }
    }

    struct InTokenBundle {
        bytes: Vec<u8>,
        buffer: Box<SecBuffer>,
        desc: SecBufferDesc,
    }

    impl InTokenBundle {
        fn new(token: &[u8]) -> Self {
            let mut bytes = token.to_vec();
            let mut buffer = Box::new(SecBuffer {
                cbBuffer: bytes.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: bytes.as_mut_ptr().cast(),
            });
            let desc = SecBufferDesc {
                ulVersion: 0,
                cBuffers: 1,
                pBuffers: buffer.as_mut(),
            };
            Self {
                bytes,
                buffer,
                desc,
            }
        }
    }

    fn acquire_cred(handle: &mut SecHandle) -> Result<()> {
        let package: Vec<u16> = "Negotiate".encode_utf16().chain([0]).collect();
        let status = unsafe {
            AcquireCredentialsHandleW(
                null_mut(),
                package.as_ptr(),
                SECPKG_CRED_OUTBOUND,
                null_mut(),
                null_mut(),
                None,
                null_mut(),
                handle,
                null_mut(),
            )
        };
        if status != SEC_E_OK {
            return Err(mechanism_error(
                AuthScheme::Negotiate,
                format!("AcquireCredentialsHandle failed with status {status}"),
            ));
        }
        Ok(())
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
}

#[cfg(all(windows, feature = "http-negotiate"))]
pub(crate) use imp::WindowsSspiContext;
