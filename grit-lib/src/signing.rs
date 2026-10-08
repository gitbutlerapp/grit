//! Commit/tag GPG (and gpgsm/ssh) signing and signature verification.
//!
//! This is a port of the parts of Git's `gpg-interface.c` and `commit.c`
//! signature handling that the grit CLI needs:
//!
//! * read `gpg.format`, `gpg.<fmt>.program` / `gpg.program`, `user.signingkey`
//!   and `gpg.minTrustLevel`,
//! * resolve the signing program (handling a leading `~`, absolute paths, and
//!   bare names looked up on `$PATH`),
//! * [`sign_buffer`] — spawn `<program> --status-fd=2 -bsau <key>` and capture
//!   the armored detached signature,
//! * [`add_header_signature`] — splice a `gpgsig` header into a serialized
//!   commit object (Git `commit.c:add_header_signature`),
//! * [`extract_signed_payload`] / [`verify_commit`] — strip the `gpgsig` header
//!   to rebuild the signed payload and run `<program> --verify` over it,
//!   parsing the `[GNUPG:]` status lines into a [`SignatureCheck`].
//!
//! Only the gpg-based formats (`openpgp` -> `gpg`, `x509` -> `gpgsm`) implement
//! signing/verification here; `ssh` is recognized for `gpg.format` validation
//! but its sign/verify paths are not exercised by the commit/verify-commit
//! tests and return an explanatory error.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::command_runner::{
    system_command_runner, CommandEnvironment, CommandOutput, CommandRunner, CommandSpec,
    CommandStdin, CommandStdio,
};
use crate::config::ConfigSet;
use crate::error::{Error, Result};

/// The hash header label for a sha1 repository.
pub const GPG_SIG_HEADER_SHA1: &str = "gpgsig";
/// The hash header label for a sha256 repository.
pub const GPG_SIG_HEADER_SHA256: &str = "gpgsig-sha256";

/// Signature trust level, as reported for a verified signature.
///
/// The numeric ordering matters: `gpg.minTrustLevel` comparisons use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TrustLevel {
    #[default]
    Undefined = 0,
    Never = 1,
    Marginal = 2,
    Fully = 3,
    Ultimate = 4,
}

impl TrustLevel {
    /// The `%GT` display string (`undefined`, `never`, ...).
    pub fn display_key(self) -> &'static str {
        match self {
            TrustLevel::Undefined => "undefined",
            TrustLevel::Never => "never",
            TrustLevel::Marginal => "marginal",
            TrustLevel::Fully => "fully",
            TrustLevel::Ultimate => "ultimate",
        }
    }

    /// Parse an uppercase GNUPG `TRUST_<LEVEL>` suffix.
    fn from_status(level: &str) -> Option<TrustLevel> {
        match level {
            "UNDEFINED" => Some(TrustLevel::Undefined),
            "NEVER" => Some(TrustLevel::Never),
            "MARGINAL" => Some(TrustLevel::Marginal),
            "FULLY" => Some(TrustLevel::Fully),
            "ULTIMATE" => Some(TrustLevel::Ultimate),
            _ => None,
        }
    }

    /// Parse a configured `gpg.minTrustLevel` value (case-insensitive).
    pub fn from_config(value: &str) -> Option<TrustLevel> {
        TrustLevel::from_status(&value.to_ascii_uppercase())
    }
}

/// The signature format selected via `gpg.format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpgFormat {
    OpenPgp,
    X509,
    Ssh,
}

impl GpgFormat {
    /// Resolve a `gpg.format` value case-sensitively (Git `get_format_by_name`).
    ///
    /// `openpgp` is valid; `OpEnPgP` is not (matches subtest 24).
    pub fn from_name(name: &str) -> Option<GpgFormat> {
        match name {
            "openpgp" => Some(GpgFormat::OpenPgp),
            "x509" => Some(GpgFormat::X509),
            "ssh" => Some(GpgFormat::Ssh),
            _ => None,
        }
    }

    /// The format name used in `gpg.<fmt>.program`.
    fn name(self) -> &'static str {
        match self {
            GpgFormat::OpenPgp => "openpgp",
            GpgFormat::X509 => "x509",
            GpgFormat::Ssh => "ssh",
        }
    }

    /// Default program for this format.
    fn default_program(self) -> &'static str {
        match self {
            GpgFormat::OpenPgp => "gpg",
            GpgFormat::X509 => "gpgsm",
            GpgFormat::Ssh => "ssh-keygen",
        }
    }

    /// Detect the format from a signature's armor header
    /// (Git `gpg-interface.c:get_format_by_sig`). Returns `None` for an
    /// unrecognized signature.
    pub fn from_signature(sig: &[u8]) -> Option<GpgFormat> {
        const OPENPGP: &[&[u8]] = &[
            b"-----BEGIN PGP SIGNATURE-----",
            b"-----BEGIN PGP MESSAGE-----",
        ];
        const X509: &[&[u8]] = &[b"-----BEGIN SIGNED MESSAGE-----"];
        const SSH: &[&[u8]] = &[b"-----BEGIN SSH SIGNATURE-----"];
        for prefix in OPENPGP {
            if sig.starts_with(prefix) {
                return Some(GpgFormat::OpenPgp);
            }
        }
        for prefix in X509 {
            if sig.starts_with(prefix) {
                return Some(GpgFormat::X509);
            }
        }
        for prefix in SSH {
            if sig.starts_with(prefix) {
                return Some(GpgFormat::Ssh);
            }
        }
        None
    }

    /// Extra arguments passed before `--verify` for this format.
    fn verify_args(self) -> &'static [&'static str] {
        match self {
            GpgFormat::OpenPgp => &["--keyid-format=long"],
            GpgFormat::X509 => &["--keyid-format=long"],
            GpgFormat::Ssh => &[],
        }
    }
}

/// Resolved signing/verification configuration.
#[derive(Clone)]
pub struct GpgConfig {
    /// The selected format.
    pub format: GpgFormat,
    /// The resolved program command for [`Self::format`] (used for signing; may
    /// be a bare name to look up on `$PATH`).
    pub program: String,
    /// `gpg.program` (the format-agnostic fallback), if set.
    pub generic_program: Option<String>,
    /// `gpg.openpgp.program`, if set.
    pub openpgp_program: Option<String>,
    /// `gpg.x509.program`, if set.
    pub x509_program: Option<String>,
    /// `gpg.ssh.program`, if set.
    pub ssh_program: Option<String>,
    /// `user.signingkey`, if set.
    pub signing_key: Option<String>,
    /// `gpg.minTrustLevel`, if set.
    pub min_trust_level: Option<TrustLevel>,
    /// `gpg.ssh.allowedSignersFile`, if set (path; leading `~/` expanded).
    pub ssh_allowed_signers: Option<String>,
    /// `gpg.ssh.revocationFile`, if set (path; leading `~/` expanded).
    pub ssh_revocation_file: Option<String>,
    /// Subprocess runner for gpg/gpgsm/ssh-signing helpers.
    pub command_runner: Arc<dyn CommandRunner>,
}

impl std::fmt::Debug for GpgConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpgConfig")
            .field("format", &self.format)
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

fn signing_spawn(
    cfg: &GpgConfig,
    spec: CommandSpec,
    payload: Option<&[u8]>,
) -> Result<CommandOutput> {
    let mut running = cfg
        .command_runner
        .spawn(&spec)
        .map_err(|e| Error::Signing(format!("could not run signing program: {e}")))?;
    if let Some(data) = payload {
        if let Some(stdin) = running.stdin_mut() {
            let _ = stdin.write_all(data);
        }
    }
    running
        .wait_with_output()
        .map_err(|e| Error::Signing(format!("failed waiting for signing program: {e}")))
}

impl GpgConfig {
    /// Read the signing configuration from a [`ConfigSet`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when `gpg.format` holds an unrecognized
    /// value (Git rejects this case-sensitively).
    pub fn from_config(config: &ConfigSet) -> Result<GpgConfig> {
        let format = match config.get("gpg.format") {
            Some(raw) => GpgFormat::from_name(&raw).ok_or_else(|| {
                Error::Config(format!("invalid value for 'gpg.format': '{raw}'").into())
            })?,
            None => GpgFormat::OpenPgp,
        };

        let nonempty = |k: &str| config.get(k).filter(|p| !p.is_empty());
        let generic_program = nonempty("gpg.program");
        let fmt_program = |f: GpgFormat| nonempty(&format!("gpg.{}.program", f.name()));
        let openpgp_program = fmt_program(GpgFormat::OpenPgp);
        let x509_program = fmt_program(GpgFormat::X509);
        let ssh_program = fmt_program(GpgFormat::Ssh);

        // `gpg.<fmt>.program` takes precedence over `gpg.program`.
        let program = resolve_program_for_format(
            format,
            generic_program.as_deref(),
            match format {
                GpgFormat::OpenPgp => openpgp_program.as_deref(),
                GpgFormat::X509 => x509_program.as_deref(),
                GpgFormat::Ssh => ssh_program.as_deref(),
            },
        );

        let signing_key = config.get("user.signingkey").filter(|k| !k.is_empty());

        let min_trust_level = config
            .get("gpg.mintrustlevel")
            .and_then(|v| TrustLevel::from_config(&v));

        // Path values: Git uses `git_config_pathname`, which expands a leading
        // `~/` relative to $HOME. `ConfigSet` lowercases section/variable names.
        let ssh_allowed_signers = config
            .get("gpg.ssh.allowedsignersfile")
            .filter(|p| !p.is_empty())
            .map(|p| expand_tilde(&p));
        let ssh_revocation_file = config
            .get("gpg.ssh.revocationfile")
            .filter(|p| !p.is_empty())
            .map(|p| expand_tilde(&p));

        Ok(GpgConfig {
            format,
            program,
            generic_program,
            openpgp_program,
            x509_program,
            ssh_program,
            signing_key,
            min_trust_level,
            ssh_allowed_signers,
            ssh_revocation_file,
            command_runner: system_command_runner(),
        })
    }

    /// Override the subprocess runner (tests and embedders).
    #[must_use]
    pub fn with_command_runner(mut self, runner: Arc<dyn CommandRunner>) -> Self {
        self.command_runner = runner;
        self
    }

    /// Resolve the program for a specific format (honoring `gpg.<fmt>.program`
    /// then `gpg.program`, falling back to the format default). Used by
    /// verification, where the format is detected from the signature armor and
    /// may differ from the configured [`Self::format`].
    fn program_for(&self, format: GpgFormat) -> String {
        let fmt_program = match format {
            GpgFormat::OpenPgp => self.openpgp_program.as_deref(),
            GpgFormat::X509 => self.x509_program.as_deref(),
            GpgFormat::Ssh => self.ssh_program.as_deref(),
        };
        resolve_program_for_format(format, self.generic_program.as_deref(), fmt_program)
    }

    /// The signing key to use: the explicit `key_override`, else
    /// `user.signingkey`, else the supplied committer identity (Git passes
    /// `git_committer_info(IDENT_STRICT | IDENT_NO_DATE)`).
    pub fn resolve_signing_key(
        &self,
        key_override: Option<&str>,
        committer_default: &str,
    ) -> String {
        if let Some(k) = key_override {
            if !k.is_empty() {
                return k.to_owned();
            }
        }
        if let Some(k) = &self.signing_key {
            return k.clone();
        }
        committer_default.to_owned()
    }

    /// Resolve [`Self::program`] to an executable path.
    ///
    /// Mirrors Git's program resolution: a leading `~/` expands to `$HOME`, an
    /// absolute path is used verbatim, and a bare name is searched on `$PATH`.
    pub fn resolve_program_path(&self) -> Result<PathBuf> {
        resolve_program(&self.program)
    }
}

/// Resolve the program *string* for `format`: `gpg.<fmt>.program` (if set),
/// else `gpg.program` (if set), else the format's built-in default.
fn resolve_program_for_format(
    format: GpgFormat,
    generic_program: Option<&str>,
    fmt_program: Option<&str>,
) -> String {
    fmt_program
        .or(generic_program)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_owned())
        .unwrap_or_else(|| format.default_program().to_owned())
}

/// Resolve a program string to an executable path.
fn resolve_program(program: &str) -> Result<PathBuf> {
    // `~` / `~/...` expansion relative to $HOME.
    if program == "~" {
        if let Some(home) = home_dir() {
            return Ok(home);
        }
    }
    if let Some(rest) = program.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return Ok(home.join(rest));
        }
    }

    let path = Path::new(program);
    // Absolute path, or any relative path that contains a separator: use as-is.
    if path.is_absolute() || program.contains('/') {
        return Ok(path.to_path_buf());
    }

    // Bare name: search $PATH.
    if let Some(found) = search_path(program) {
        return Ok(found);
    }

    // Fall back to the bare name and let the OS resolve it (preserves Git's
    // behavior of handing the name straight to exec when not found on PATH).
    Ok(path.to_path_buf())
}

/// Look up a bare program name on `$PATH`.
fn search_path(name: &str) -> Option<PathBuf> {
    let paths = crate::environment::Environment::empty().var_os("PATH")?;
    for dir in paths.to_string_lossy().split(':').map(PathBuf::from) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// The user's home directory (`$HOME`).
fn home_dir() -> Option<PathBuf> {
    crate::environment::Environment::empty()
        .var_os("HOME")
        .map(PathBuf::from)
}

/// Expand a leading `~/` (and a bare `~`) relative to `$HOME`, like Git's
/// `interpolate_path` / `git_config_pathname` for the simple home case.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        if let Some(home) = home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_owned()
}

/// Sign `payload` with `signing_key` using the configured program.
///
/// Spawns `<program> --status-fd=2 -bsau <signing_key>`, writes `payload` to
/// stdin, and returns the armored detached signature from stdout.  Fails if the
/// child exits non-zero or does not emit a `[GNUPG:] SIG_CREATED` status line —
/// in either case the program's stderr is surfaced in the error (the
/// `LET_GPG_PROGRAM_FAIL`/`zOMG` path of subtest 28).
///
/// # Errors
///
/// Returns [`Error::Signing`] when the program cannot be spawned, exits
/// non-zero, or fails to produce a signature.
pub fn sign_buffer(cfg: &GpgConfig, payload: &[u8], signing_key: &str) -> Result<Vec<u8>> {
    if cfg.format == GpgFormat::Ssh {
        return sign_buffer_ssh(cfg, payload, signing_key);
    }

    let program = cfg.resolve_program_path()?;

    let spec = CommandSpec {
        program: program.clone(),
        args: vec![
            OsString::from("--status-fd=2"),
            OsString::from("-bsau"),
            OsString::from(signing_key),
        ],
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Pipe,
    };
    let output = signing_spawn(cfg, spec, Some(payload))?;

    let status_text = String::from_utf8_lossy(&output.stderr);

    if !output.status.success || !has_sig_created(&status_text) {
        let detail = if status_text.trim().is_empty() {
            "(no gpg output)".to_owned()
        } else {
            status_text.into_owned()
        };
        return Err(Error::Signing(format!(
            "gpg failed to sign the data:\n{detail}"
        )));
    }

    Ok(output.stdout)
}

/// True when a `[GNUPG:] SIG_CREATED ` status line is present at the start of a
/// line (Git's `sign_buffer_gpg` SIG_CREATED scan).
fn has_sig_created(status: &str) -> bool {
    status
        .lines()
        .any(|line| line.starts_with("[GNUPG:] SIG_CREATED "))
}

/// Detect a literal ssh key (Git `gpg-interface.c:is_literal_ssh_key`).
///
/// Returns `Some(rest)` for `key::<rest>`, `Some(s)` for a value starting with
/// `ssh-`, else `None`.
fn is_literal_ssh_key(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("key::") {
        return Some(rest);
    }
    if s.starts_with("ssh-") {
        return Some(s);
    }
    None
}

/// Sign `payload` with an ssh key using `ssh-keygen -Y sign`.
///
/// Port of Git's `gpg-interface.c:sign_buffer_ssh`.  `signing_key` is either a
/// literal public key (`key::...` or `ssh-...`) or a path to a key file
/// (`~/` expanded).  Returns the armored `-----BEGIN SSH SIGNATURE-----` blob.
///
/// # Errors
///
/// Returns [`Error::Signing`] when `signing_key` is empty, a temp file cannot be
/// written, `ssh-keygen` cannot be run or exits non-zero, or the `.sig` output
/// cannot be read.
fn sign_buffer_ssh(cfg: &GpgConfig, payload: &[u8], signing_key: &str) -> Result<Vec<u8>> {
    if signing_key.is_empty() {
        return Err(Error::Signing(
            "user.signingKey needs to be set for ssh signing".to_owned(),
        ));
    }

    let program = cfg.resolve_program_path()?;

    // Resolve the key file: either a literal key written to a temp file (with
    // the `-U` flag), or a path on disk.
    let mut literal_key_tmp: Option<PathBuf> = None;
    let (key_file, literal): (String, bool) = match is_literal_ssh_key(signing_key) {
        Some(literal_key) => {
            let path = write_temp_file_named(literal_key.as_bytes(), "git_signing_key")?;
            let p = path.to_string_lossy().into_owned();
            literal_key_tmp = Some(path);
            (p, true)
        }
        None => (expand_tilde(signing_key), false),
    };

    // Write the payload to a temp buffer file; ssh-keygen reads it as the file
    // to sign and writes `<file>.sig` alongside it.
    let buffer_path = match write_temp_file_named(payload, "git_signing_buffer") {
        Ok(p) => p,
        Err(e) => {
            if let Some(p) = &literal_key_tmp {
                let _ = std::fs::remove_file(p);
            }
            return Err(e);
        }
    };

    let mut args = vec![
        OsString::from("-Y"),
        OsString::from("sign"),
        OsString::from("-n"),
        OsString::from("git"),
        OsString::from("-f"),
        OsString::from(key_file.as_str()),
    ];
    if literal {
        args.push(OsString::from("-U"));
    }
    args.push(OsString::from(buffer_path.to_string_lossy().as_ref()));
    let spec = CommandSpec {
        program: program.clone(),
        args,
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Pipe,
    };

    let cleanup = |literal_key_tmp: &Option<PathBuf>, buffer_path: &Path| {
        if let Some(p) = literal_key_tmp {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_file(buffer_path);
        let _ = std::fs::remove_file(sig_sibling(buffer_path));
    };

    let output = match signing_spawn(cfg, spec, None) {
        Ok(o) => o,
        Err(e) => {
            cleanup(&literal_key_tmp, &buffer_path);
            return Err(e);
        }
    };

    if !output.status.success {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup(&literal_key_tmp, &buffer_path);
        if stderr.contains("usage:") {
            return Err(Error::Signing(
                "ssh-keygen -Y sign is needed for ssh signing (available in openssh version 8.2p1+)"
                    .to_owned(),
            ));
        }
        return Err(Error::Signing(stderr.into_owned()));
    }

    let sig_path = sig_sibling(&buffer_path);
    let result = std::fs::read(&sig_path).map_err(|e| {
        Error::Signing(format!(
            "failed reading ssh signing data buffer from '{}': {e}",
            sig_path.display()
        ))
    });
    cleanup(&literal_key_tmp, &buffer_path);

    let mut sig = result?;
    // Strip a trailing CR (Windows line endings) from each line, mirroring
    // Git's `remove_cr_after`.
    strip_cr(&mut sig);
    Ok(sig)
}

/// The `<path>.sig` sibling file produced by `ssh-keygen -Y sign`.
fn sig_sibling(buffer_path: &Path) -> PathBuf {
    let mut s = buffer_path.as_os_str().to_owned();
    s.push(".sig");
    PathBuf::from(s)
}

/// Remove `\r` characters that immediately precede a `\n` (Git `remove_cr_after`).
fn strip_cr(buf: &mut Vec<u8>) {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\r' && buf.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(buf[i]);
        i += 1;
    }
    *buf = out;
}

/// Splice a signature into a serialized commit object as a `gpgsig` header.
///
/// Port of Git's `commit.c:add_header_signature`: find the first `\n\n`, insert
/// the header at the position right after that first `\n`, prefix the first
/// signature line with `<header> ` and every subsequent line with a single
/// space.  `header` is [`GPG_SIG_HEADER_SHA1`] or [`GPG_SIG_HEADER_SHA256`].
pub fn add_header_signature(buf: &[u8], sig: &[u8], header: &str) -> Vec<u8> {
    // Find end of header (first occurrence of "\n\n"); inspos is just past the
    // first '\n'. If absent, append at the end.
    let inspos = find_double_newline(buf).map(|p| p + 1).unwrap_or(buf.len());

    let mut out = Vec::with_capacity(buf.len() + sig.len() + header.len() + 16);
    out.extend_from_slice(&buf[..inspos]);

    let mut first = true;
    let mut copypos = 0usize;
    while copypos < sig.len() {
        let bol = copypos;
        // End of this line, including the trailing '\n' when present.
        let end = match memchr(sig, copypos, b'\n') {
            Some(idx) => idx + 1,
            None => sig.len(),
        };

        if first {
            out.extend_from_slice(header.as_bytes());
            first = false;
        }
        out.push(b' ');
        out.extend_from_slice(&sig[bol..end]);
        copypos = end;
    }

    out.extend_from_slice(&buf[inspos..]);
    out
}

/// Find the first `\n\n` in `buf`, returning the index of the first `\n`.
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find the byte `needle` in `buf` starting at `from`.
fn memchr(buf: &[u8], from: usize, needle: u8) -> Option<usize> {
    buf.get(from..)
        .and_then(|s| s.iter().position(|&b| b == needle))
        .map(|p| p + from)
}

/// A parsed signature check result.
#[derive(Debug, Clone, Default)]
pub struct SignatureCheck {
    /// The detached armored signature extracted from the object.
    pub signature: Vec<u8>,
    /// The signed payload (object with `gpgsig` header removed).
    pub payload: Vec<u8>,
    /// `%G?` result: `G` good, `B` bad, `U` good+untrusted, `E` error,
    /// `N` no signature, `X`/`Y`/`R` expired/expired-key/revoked.
    pub result: char,
    /// `%GT` trust level.
    pub trust_level: TrustLevel,
    /// `%GK` key id.
    pub key: Option<String>,
    /// `%GS` signer (uid).
    pub signer: Option<String>,
    /// `%GF` signing key fingerprint.
    pub fingerprint: Option<String>,
    /// `%GP` primary key fingerprint.
    pub primary_key_fingerprint: Option<String>,
    /// Human-readable gpg output (stderr); shown by `--show-signature`.
    pub output: String,
    /// Raw `[GNUPG:]` status lines; shown by `verify-commit --raw`.
    pub gpg_status: String,
    /// True when the underlying verifier reported failure regardless of the
    /// parsed `%G?` result.  For ssh this captures Git's
    /// `verify_ssh_signed_buffer` return code (e.g. an untrusted key that still
    /// produces a `Good "git" signature with ...` line must fail verification).
    pub verifier_failed: bool,
}

impl SignatureCheck {
    /// Construct the "no signature" result (`%G?` -> `N`).
    pub fn default_none() -> SignatureCheck {
        SignatureCheck {
            result: 'N',
            trust_level: TrustLevel::Undefined,
            ..Default::default()
        }
    }

    /// True when the signature verified as good (`G`) or good-but-expired-key
    /// (`Y`) — Git's success criterion in `check_signature`.
    pub fn is_good(&self) -> bool {
        self.result == 'G' || self.result == 'Y'
    }

    /// Overall verification result honoring `min_trust_level`: `Ok(())` when the
    /// signature is good and meets the configured minimum trust level.
    pub fn verify_status(&self, min_trust_level: Option<TrustLevel>) -> bool {
        if self.verifier_failed {
            return false;
        }
        if !self.is_good() {
            return false;
        }
        if let Some(min) = min_trust_level {
            if self.trust_level < min {
                return false;
            }
        }
        true
    }
}

/// Extract the `gpgsig` (or `gpgsig-sha256`) header value and the signed
/// payload from a raw commit object.
///
/// Returns `(payload, signature)` where `payload` is the commit object with the
/// `gpgsig` header removed (the bytes that were actually signed), and
/// `signature` is the de-indented armored signature.  Returns `None` when the
/// object carries no signature header.
pub fn extract_signed_payload(raw_commit: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    // Header region ends at first "\n\n".
    let header_end = find_double_newline(raw_commit)?;
    let header = &raw_commit[..=header_end]; // includes trailing first '\n'
    let body = &raw_commit[header_end + 1..]; // remaining (starts with '\n')

    let mut payload = Vec::with_capacity(raw_commit.len());
    let mut signature = Vec::new();
    let mut found = false;

    let mut idx = 0;
    while idx < header.len() {
        let line_end = memchr(header, idx, b'\n')
            .map(|p| p + 1)
            .unwrap_or(header.len());
        let line = &header[idx..line_end];

        let is_sig_header = line.starts_with(GPG_SIG_HEADER_SHA1.as_bytes())
            && line
                .get(GPG_SIG_HEADER_SHA1.len())
                .map(|&b| b == b' ')
                .unwrap_or(false);

        if is_sig_header && !found {
            found = true;
            // First signature line: text after "gpgsig ".
            let prefix_len = GPG_SIG_HEADER_SHA1.len() + 1;
            signature.extend_from_slice(&line[prefix_len..]);
            idx = line_end;
            // Subsequent continuation lines (leading space) belong to the sig.
            while idx < header.len() {
                let cont_end = memchr(header, idx, b'\n')
                    .map(|p| p + 1)
                    .unwrap_or(header.len());
                let cont = &header[idx..cont_end];
                if cont.first() == Some(&b' ') {
                    signature.extend_from_slice(&cont[1..]);
                    idx = cont_end;
                } else {
                    break;
                }
            }
            continue;
        }

        payload.extend_from_slice(line);
        idx = line_end;
    }

    if !found {
        return None;
    }

    payload.extend_from_slice(body);
    Some((payload, signature))
}

/// Verify a raw commit object's embedded signature.
///
/// Extracts the payload + signature, then (for gpg-based formats) writes the
/// signature to a temp file and runs `<program> --status-fd=1 <verify_args>
/// --verify <sigfile> -`, feeding the payload on stdin, and parses the
/// `[GNUPG:]` status lines.
pub fn verify_commit(cfg: &GpgConfig, raw_commit: &[u8]) -> Result<SignatureCheck> {
    let (payload, signature) = match extract_signed_payload(raw_commit) {
        Some(parts) => parts,
        None => return Ok(SignatureCheck::default_none()),
    };

    // Git picks the verifier from the *signature* armor (`get_format_by_sig`),
    // not from `gpg.format`: a `git verify-commit` over an ssh-signed commit must
    // use ssh-keygen even when `gpg.format` is unset/openpgp, and vice-versa.
    let detected_format = GpgFormat::from_signature(&signature).unwrap_or(cfg.format);

    if detected_format == GpgFormat::Ssh {
        return verify_ssh_signed_buffer(cfg, payload, signature);
    }

    let program = resolve_program(&cfg.program_for(detected_format))?;

    // Write the detached signature to a temp file.
    let sig_path = write_temp_file(&signature)?;

    let mut args = vec![OsString::from("--status-fd=1")];
    for a in detected_format.verify_args() {
        args.push(OsString::from(a));
    }
    args.push(OsString::from("--verify"));
    args.push(OsString::from(sig_path.to_string_lossy().as_ref()));
    args.push(OsString::from("-"));
    let spec = CommandSpec {
        program: program.clone(),
        args,
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Pipe,
    };

    let output = match signing_spawn(cfg, spec, Some(&payload)) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&sig_path);
            return Err(e);
        }
    };
    let _ = std::fs::remove_file(&sig_path);

    // status-fd=1 routes GNUPG status to stdout; human-readable goes to stderr.
    let gpg_status = String::from_utf8_lossy(&output.stdout).into_owned();
    let human = String::from_utf8_lossy(&output.stderr).into_owned();

    let mut sigc = SignatureCheck {
        signature,
        payload,
        result: 'N',
        trust_level: TrustLevel::Undefined,
        gpg_status: gpg_status.clone(),
        output: human,
        ..Default::default()
    };

    parse_gpg_output(&mut sigc, &gpg_status);

    Ok(sigc)
}

/// Parse `ssh-keygen -Y verify` human output into `sigc`
/// (produces the same parse as Git's `gpg-interface.c:parse_ssh_output`).
///
/// Expected first line of `sigc.output`:
/// * `Good "git" signature for PRINCIPAL with ... key SHA256:FINGERPRINT`
///   -> result `G`, trust `Fully`, signer = PRINCIPAL.
/// * `Good "git" signature with ... key SHA256:FINGERPRINT`
///   -> result `G`, trust `Undefined` (unknown key, signer unset).
/// * anything else -> result `B`, trust `Never`.
///
/// In the two good cases the substring after `key ` becomes both the
/// fingerprint (`%GF`) and the key (`%GK`); `%GP` is never set.
fn parse_ssh_output(sigc: &mut SignatureCheck) {
    sigc.result = 'B';
    sigc.trust_level = TrustLevel::Never;
    sigc.key = None;
    sigc.signer = None;
    sigc.fingerprint = None;
    sigc.primary_key_fingerprint = None;

    let first_line = sigc.output.split('\n').next().unwrap_or("");

    let after_key;
    if let Some(rest) = first_line.strip_prefix("Good \"git\" signature for ") {
        // The principal can contain whitespace; the trailing
        // ` with <algo> key <fpr>` is fixed, so split on the *last* " with ".
        match rest.rfind(" with ") {
            Some(idx) => {
                let principal = &rest[..idx];
                if principal.is_empty() {
                    return;
                }
                sigc.result = 'G';
                sigc.trust_level = TrustLevel::Fully;
                sigc.signer = Some(principal.to_owned());
                after_key = &rest[idx + " with ".len()..];
            }
            None => return,
        }
    } else if let Some(rest) = first_line.strip_prefix("Good \"git\" signature with ") {
        sigc.result = 'G';
        sigc.trust_level = TrustLevel::Undefined;
        after_key = rest;
    } else {
        return;
    }

    // The fingerprint follows the literal `key ` token.
    match after_key.find("key ") {
        Some(pos) => {
            let fpr = after_key[pos + "key ".len()..].to_owned();
            sigc.fingerprint = Some(fpr.clone());
            sigc.key = Some(fpr);
        }
        None => {
            // Output did not match what we expected: treat as bad.
            sigc.result = 'B';
        }
    }
}

/// Extract the committer (or tagger) unix timestamp from a signed payload,
/// porting Git's `parse_payload_metadata` for the `committer`/`tagger` header.
fn payload_committer_timestamp(payload: &[u8]) -> Option<u64> {
    let ident_line =
        find_header_line(payload, b"committer").or_else(|| find_header_line(payload, b"tagger"))?;
    let line = std::str::from_utf8(ident_line).ok()?;
    // Ident line: "Name <email> <timestamp> <tz>"; the timestamp is the
    // second-to-last whitespace-separated token.
    let mut it = line.split_whitespace().rev();
    let _tz = it.next()?;
    let ts = it.next()?;
    ts.parse::<u64>().ok()
}

/// Return the bytes after `"<name> "` for the first header line matching `name`
/// (within the header region, i.e. before the first blank line).
fn find_header_line<'a>(payload: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut idx = 0;
    while idx < payload.len() {
        let line_end = memchr(payload, idx, b'\n').unwrap_or(payload.len());
        let line = &payload[idx..line_end];
        if line.is_empty() {
            // End of header region.
            return None;
        }
        if line.len() > name.len() + 1 && &line[..name.len()] == name && line[name.len()] == b' ' {
            return Some(&line[name.len() + 1..]);
        }
        idx = line_end + 1;
    }
    None
}

/// Format a `-Overify-time=YYYYMMDDhhmmss` argument from a unix timestamp using
/// the local timezone, mirroring Git's `verify_date_mode` (DATE_STRFTIME, local).
fn verify_time_arg(timestamp: u64) -> String {
    use crate::git_date::show::{show_date, DateMode, DateModeType};
    let mut mode = DateMode {
        ty: DateModeType::Strftime,
        local: true,
        strftime_fmt: Some("%Y%m%d%H%M%S".to_owned()),
    };
    let formatted = show_date(timestamp, 0, &mut mode);
    format!("-Overify-time={formatted}")
}

/// Verify an ssh-signed `payload` against `signature` using `ssh-keygen -Y`.
///
/// Port of Git's `gpg-interface.c:verify_ssh_signed_buffer`.  Requires
/// `gpg.ssh.allowedSignersFile`; runs `find-principals`, then either
/// `check-novalidate` (no principal matched -> untrusted) or `verify` for each
/// matched principal.  Populates `sigc.output`/`sigc.gpg_status` and parses them
/// via [`parse_ssh_output`].
fn verify_ssh_signed_buffer(
    cfg: &GpgConfig,
    payload: Vec<u8>,
    signature: Vec<u8>,
) -> Result<SignatureCheck> {
    let mut sigc = SignatureCheck {
        signature: signature.clone(),
        payload: payload.clone(),
        result: 'N',
        trust_level: TrustLevel::Undefined,
        ..Default::default()
    };

    let allowed = match &cfg.ssh_allowed_signers {
        Some(a) if !a.is_empty() => a.clone(),
        _ => {
            sigc.result = 'B';
            sigc.trust_level = TrustLevel::Never;
            sigc.output = "gpg.ssh.allowedSignersFile needs to be configured and exist for ssh signature verification".to_owned();
            sigc.gpg_status = sigc.output.clone();
            return Ok(sigc);
        }
    };

    // The format here is detected from the signature armor, which may differ
    // from `cfg.format`; always resolve the ssh program (`gpg.ssh.program` /
    // `gpg.program` / `ssh-keygen`).
    let program = resolve_program(&cfg.program_for(GpgFormat::Ssh))?;

    // Write the detached signature to a temp `.git_vtag` file.
    let sig_path = write_temp_file_named(&signature, "git_vtag")?;

    let verify_time = payload_committer_timestamp(&payload).map(verify_time_arg);

    // 1. find-principals: which allowed principals can verify this signature?
    let mut find_args = vec![
        OsString::from("-Y"),
        OsString::from("find-principals"),
        OsString::from("-f"),
        OsString::from(allowed.as_str()),
        OsString::from("-s"),
        OsString::from(sig_path.to_string_lossy().as_ref()),
    ];
    if let Some(vt) = &verify_time {
        find_args.push(OsString::from(vt.as_str()));
    }
    let find_spec = CommandSpec {
        program: program.clone(),
        args: find_args,
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Pipe,
    };
    let find_out = signing_spawn(cfg, find_spec, None).inspect_err(|_| {
        let _ = std::fs::remove_file(&sig_path);
    })?;

    let find_stdout = String::from_utf8_lossy(&find_out.stdout).into_owned();
    let find_stderr = String::from_utf8_lossy(&find_out.stderr).into_owned();

    if !find_out.status.success && find_stderr.contains("usage:") {
        let _ = std::fs::remove_file(&sig_path);
        return Err(Error::Signing(
            "ssh-keygen -Y find-principals/verify is needed for ssh signature verification (available in openssh version 8.2p1+)"
                .to_owned(),
        ));
    }

    let mut verify_stdout = String::new();
    let mut verify_stderr = String::new();
    // Tracks Git's `ret` in verify_ssh_signed_buffer: true means failure.
    let mut verifier_failed;

    if !find_out.status.success || find_stdout.trim().is_empty() {
        // No matching principal: run check-novalidate to surface signature info,
        // but treat as untrusted (Git forces ret = -1).
        let mut check_args = vec![
            OsString::from("-Y"),
            OsString::from("check-novalidate"),
            OsString::from("-n"),
            OsString::from("git"),
            OsString::from("-s"),
            OsString::from(sig_path.to_string_lossy().as_ref()),
        ];
        if let Some(vt) = &verify_time {
            check_args.push(OsString::from(vt.as_str()));
        }
        let (out, err) = run_with_stdin(cfg, &program, check_args, &payload);
        verify_stdout = out;
        verify_stderr = err;
        verifier_failed = true;
    } else {
        // Try each matched principal until one verifies as Good.
        verifier_failed = true;
        for principal in find_stdout.lines() {
            let principal = principal.trim_end_matches('\r');
            if principal.is_empty() {
                continue;
            }
            let mut verify_args = vec![
                OsString::from("-Y"),
                OsString::from("verify"),
                OsString::from("-n"),
                OsString::from("git"),
                OsString::from("-f"),
                OsString::from(allowed.as_str()),
                OsString::from("-I"),
                OsString::from(principal),
                OsString::from("-s"),
                OsString::from(sig_path.to_string_lossy().as_ref()),
            ];
            if let Some(vt) = &verify_time {
                verify_args.push(OsString::from(vt.as_str()));
            }
            if let Some(rev) = &cfg.ssh_revocation_file {
                if Path::new(rev).exists() {
                    verify_args.push(OsString::from("-r"));
                    verify_args.push(OsString::from(rev.as_str()));
                }
            }
            let (out, err, ok) = run_with_stdin_status(cfg, &program, verify_args, &payload);
            verify_stdout = out;
            verify_stderr = err;
            // Git: ret = !ok; if !ret { ret = !starts_with("Good"); }
            verifier_failed = !(ok && verify_stdout.starts_with("Good"));
            if !verifier_failed {
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&sig_path);

    // Build sigc.output exactly as Git: stripspace the ssh stdout and stderr
    // (each non-empty line keeps a trailing newline), then append the
    // find-principals stderr and the verify/check stderr (gpg-interface.c
    // 601-608). The trailing newline left by stripspace keeps the `Good "..."`
    // line separate from any appended `No principal matched.` text so
    // parse_ssh_output sees a clean first line.
    let mut output = stripspace(&verify_stdout);
    let verify_stderr = stripspace(&verify_stderr);
    output.push_str(&find_stderr);
    output.push_str(&verify_stderr);

    sigc.output = output;
    sigc.gpg_status = sigc.output.clone();
    parse_ssh_output(&mut sigc);
    // Git combines the verifier return code with the parsed result/trust; the
    // parse already drives result/trust, so just carry the verifier failure.
    sigc.verifier_failed = verifier_failed;

    Ok(sigc)
}

/// Run `program` with `args`, feeding `input` on stdin.
fn run_with_stdin(
    cfg: &GpgConfig,
    program: &Path,
    args: Vec<OsString>,
    input: &[u8],
) -> (String, String) {
    let (out, err, _ok) = run_with_stdin_status(cfg, program, args, input);
    (out, err)
}

/// Like [`run_with_stdin`] but also returns whether the child exited zero.
fn run_with_stdin_status(
    cfg: &GpgConfig,
    program: &Path,
    args: Vec<OsString>,
    input: &[u8],
) -> (String, String, bool) {
    let spec = CommandSpec {
        program: program.to_path_buf(),
        args,
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Pipe,
    };
    match signing_spawn(cfg, spec, Some(input)) {
        Ok(o) => (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.status.success,
        ),
        Err(_) => (String::new(), String::new(), false),
    }
}

/// Port of Git's `strbuf_stripspace` (without comment handling): trim trailing
/// whitespace from each line, collapse runs of blank lines to a single blank
/// line, and terminate every non-empty line with a single `\n`. The retained
/// trailing newline is what keeps the ssh `Good "..."` line separate from the
/// appended `No principal matched.` stderr.
fn stripspace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_empties = 0usize;
    let mut wrote_any = false;
    for line in s.split('\n') {
        let trimmed = line.trim_end_matches([' ', '\t', '\r']);
        if trimmed.is_empty() {
            pending_empties += 1;
            continue;
        }
        if pending_empties > 0 && wrote_any {
            out.push('\n');
        }
        pending_empties = 0;
        out.push_str(trimmed);
        out.push('\n');
        wrote_any = true;
    }
    out
}

/// Parse `[GNUPG:]` status lines into `sigc` (port of `parse_gpg_output`).
fn parse_gpg_output(sigc: &mut SignatureCheck, status: &str) {
    // (result-char, prefix, exclusive, keyid, uid, fingerprint, trust)
    struct Entry {
        result: Option<char>,
        check: &'static str,
        exclusive: bool,
        keyid: bool,
        uid: bool,
        fingerprint: bool,
        trust: bool,
    }
    const TABLE: &[Entry] = &[
        Entry {
            result: Some('G'),
            check: "GOODSIG ",
            exclusive: true,
            keyid: true,
            uid: true,
            fingerprint: false,
            trust: false,
        },
        Entry {
            result: Some('B'),
            check: "BADSIG ",
            exclusive: true,
            keyid: true,
            uid: true,
            fingerprint: false,
            trust: false,
        },
        Entry {
            result: Some('E'),
            check: "ERRSIG ",
            exclusive: true,
            keyid: true,
            uid: false,
            fingerprint: false,
            trust: false,
        },
        Entry {
            result: Some('X'),
            check: "EXPSIG ",
            exclusive: true,
            keyid: true,
            uid: true,
            fingerprint: false,
            trust: false,
        },
        Entry {
            result: Some('Y'),
            check: "EXPKEYSIG ",
            exclusive: true,
            keyid: true,
            uid: true,
            fingerprint: false,
            trust: false,
        },
        Entry {
            result: Some('R'),
            check: "REVKEYSIG ",
            exclusive: true,
            keyid: true,
            uid: true,
            fingerprint: false,
            trust: false,
        },
        Entry {
            result: None,
            check: "VALIDSIG ",
            exclusive: false,
            keyid: false,
            uid: false,
            fingerprint: true,
            trust: false,
        },
        Entry {
            result: None,
            check: "TRUST_",
            exclusive: false,
            keyid: false,
            uid: false,
            fingerprint: false,
            trust: true,
        },
    ];

    let mut seen_exclusive = false;

    for raw_line in status.lines() {
        let line = match raw_line.strip_prefix("[GNUPG:] ") {
            Some(l) => l,
            None => continue,
        };

        for entry in TABLE {
            let rest = match line.strip_prefix(entry.check) {
                Some(r) => r,
                None => continue,
            };

            if entry.exclusive {
                if seen_exclusive {
                    // Multiple exclusive statuses => multiple signatures: reject.
                    error_reset(sigc);
                    return;
                }
                seen_exclusive = true;
            }

            if let Some(r) = entry.result {
                sigc.result = r;
            }

            let mut cursor = rest;

            if entry.keyid {
                let (key, after) = split_at_space(cursor);
                sigc.key = Some(key.to_owned());
                if entry.uid && !after.is_empty() {
                    // signer is the rest of the line.
                    let signer = after.split('\n').next().unwrap_or("");
                    sigc.signer = Some(signer.to_owned());
                }
            }

            if entry.trust {
                let level: String = cursor
                    .chars()
                    .take_while(|&c| c != ' ' && c != '\n')
                    .collect();
                match TrustLevel::from_status(&level) {
                    Some(t) => sigc.trust_level = t,
                    None => {
                        error_reset(sigc);
                        return;
                    }
                }
            }

            if entry.fingerprint {
                // VALIDSIG <fingerprint> ... <primary-fingerprint>
                let (fpr, mut after) = split_at_space(cursor);
                sigc.fingerprint = Some(fpr.to_owned());
                // Skip 9 interim fields to reach the primary fingerprint.
                cursor = after;
                let mut remaining = 9;
                while remaining > 0 && !cursor.is_empty() {
                    let (_, next) = split_at_space(cursor);
                    after = next;
                    if after.is_empty() {
                        break;
                    }
                    cursor = after;
                    remaining -= 1;
                }
                if remaining == 0 {
                    let primary = cursor.split('\n').next().unwrap_or("");
                    sigc.primary_key_fingerprint = Some(primary.to_owned());
                }
            }

            break;
        }
    }
}

/// Reset `sigc` to the error state, clearing partial fields.
fn error_reset(sigc: &mut SignatureCheck) {
    sigc.result = 'E';
    sigc.primary_key_fingerprint = None;
    sigc.fingerprint = None;
    sigc.signer = None;
    sigc.key = None;
}

/// Split `s` at the first space, returning `(before, after_space)`.
fn split_at_space(s: &str) -> (&str, &str) {
    match s.find(' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

/// Write `data` to a fresh temp file and return its path.
fn write_temp_file(data: &[u8]) -> Result<PathBuf> {
    write_temp_file_named(data, "git_vtag")
}

/// Build a fresh, reasonably unique temp path with the given name stem (without
/// creating the file).
fn temp_file_path(stem: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    let unique = format!("{stem}_{}_{}", std::process::id(), next_temp_counter());
    dir.join(unique)
}

/// Write `data` to a fresh temp file named with `stem` and return its path.
fn write_temp_file_named(data: &[u8], stem: &str) -> Result<PathBuf> {
    let path = temp_file_path(stem);
    let mut f = std::fs::File::create(&path)
        .map_err(|e| Error::Signing(format!("could not create temporary file: {e}")))?;
    f.write_all(data)
        .map_err(|e| Error::Signing(format!("failed writing to temporary file: {e}")))?;
    Ok(path)
}

/// Monotonic counter used to disambiguate temp file names within a process.
fn next_temp_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = crate::git_date::tm::process_wall_clock_sec() as u64;
    now ^ COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Build the committer-info default signing key (Git's
/// `git_committer_info(IDENT_STRICT | IDENT_NO_DATE)` — `"Name <email>"`).
///
/// `committer_ident` is a full ident line (`"Name <email> <ts> <tz>"`); this
/// trims the trailing timestamp/timezone.
pub fn committer_signing_default(committer_ident: &str) -> String {
    if let Some(angle_end) = committer_ident.find('>') {
        committer_ident[..=angle_end].to_owned()
    } else {
        committer_ident.to_owned()
    }
}

/// Split a signed buffer (e.g. a tag object) into `(payload, signature)`.
///
/// Port of Git's `gpg-interface.c:parse_signed_buffer`/`parse_signature`: scans
/// the buffer line by line and records the offset of the *last* line that begins
/// a recognized signature armor ([`GpgFormat::from_signature`]).  The payload is
/// everything before that offset and the signature is everything from it to the
/// end.  Unlike commits, a signed tag appends the armored signature directly
/// after the tag body with no `gpgsig` header and no per-line indentation.
///
/// Returns `None` when no armor line is found (the buffer is unsigned).
pub fn parse_signed_buffer(buf: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let size = buf.len();
    let mut len = 0usize;
    let mut matched: Option<usize> = None;
    while len < size {
        if GpgFormat::from_signature(&buf[len..]).is_some() {
            matched = Some(len);
        }
        let eol = memchr(buf, len, b'\n');
        len = match eol {
            Some(p) => p + 1,
            None => size,
        };
    }
    let m = matched?;
    Some((buf[..m].to_vec(), buf[m..].to_vec()))
}

/// Verify a raw tag object's appended signature.
///
/// Mirrors [`verify_commit`] but uses [`parse_signed_buffer`] (tag signatures are
/// appended, not stored in a `gpgsig` header).  The verifier is chosen from the
/// signature armor (`get_format_by_sig`), reusing the internal `verify_ssh_signed_buffer` helper
/// for ssh and the gpg/gpgsm path otherwise.  Returns the "no signature" result
/// when the tag carries no signature.
pub fn verify_tag(cfg: &GpgConfig, raw_tag: &[u8]) -> Result<SignatureCheck> {
    let (payload, signature) = match parse_signed_buffer(raw_tag) {
        Some(parts) => parts,
        None => return Ok(SignatureCheck::default_none()),
    };

    let detected_format = GpgFormat::from_signature(&signature).unwrap_or(cfg.format);

    if detected_format == GpgFormat::Ssh {
        return verify_ssh_signed_buffer(cfg, payload, signature);
    }

    let program = resolve_program(&cfg.program_for(detected_format))?;

    let sig_path = write_temp_file(&signature)?;

    let mut args = vec![OsString::from("--status-fd=1")];
    for a in detected_format.verify_args() {
        args.push(OsString::from(a));
    }
    args.push(OsString::from("--verify"));
    args.push(OsString::from(sig_path.to_string_lossy().as_ref()));
    args.push(OsString::from("-"));
    let spec = CommandSpec {
        program: program.clone(),
        args,
        shell: None,
        cwd: None,
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Pipe,
    };

    let output = match signing_spawn(cfg, spec, Some(&payload)) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&sig_path);
            return Err(e);
        }
    };
    let _ = std::fs::remove_file(&sig_path);

    let gpg_status = String::from_utf8_lossy(&output.stdout).into_owned();
    let human = String::from_utf8_lossy(&output.stderr).into_owned();

    let mut sigc = SignatureCheck {
        signature,
        payload,
        result: 'N',
        trust_level: TrustLevel::Undefined,
        gpg_status: gpg_status.clone(),
        output: human,
        ..Default::default()
    };

    parse_gpg_output(&mut sigc, &gpg_status);

    Ok(sigc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_name_is_case_sensitive() {
        assert_eq!(GpgFormat::from_name("openpgp"), Some(GpgFormat::OpenPgp));
        assert_eq!(GpgFormat::from_name("x509"), Some(GpgFormat::X509));
        assert_eq!(GpgFormat::from_name("ssh"), Some(GpgFormat::Ssh));
        assert_eq!(GpgFormat::from_name("OpEnPgP"), None);
        assert_eq!(GpgFormat::from_name("OPENPGP"), None);
    }

    #[test]
    fn add_header_signature_splices_gpgsig() {
        let commit = b"tree 0123\nparent 4567\nauthor a\ncommitter c\n\nmessage\n";
        let sig = b"-----BEGIN PGP SIGNATURE-----\nABC\n-----END PGP SIGNATURE-----\n";
        let out = add_header_signature(commit, sig, GPG_SIG_HEADER_SHA1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\ncommitter c\ngpgsig -----BEGIN PGP SIGNATURE-----\n ABC\n -----END PGP SIGNATURE-----\n\nmessage\n"));
    }

    #[test]
    fn extract_round_trips_signature() {
        let commit = b"tree 0123\nparent 4567\nauthor a\ncommitter c\n\nmessage\n";
        let sig = b"-----BEGIN PGP SIGNATURE-----\nABC\n-----END PGP SIGNATURE-----\n";
        let signed = add_header_signature(commit, sig, GPG_SIG_HEADER_SHA1);
        let (payload, extracted) = extract_signed_payload(&signed).unwrap();
        assert_eq!(payload, commit);
        assert_eq!(extracted, sig);
    }

    #[test]
    fn extract_none_when_unsigned() {
        let commit = b"tree 0123\ncommitter c\n\nmsg\n";
        assert!(extract_signed_payload(commit).is_none());
    }

    #[test]
    fn parse_signed_buffer_splits_appended_tag_signature() {
        // A signed tag appends the armored signature directly after the body
        // with no `gpgsig` header and no per-line indentation.
        let body = b"object 0123\ntype commit\ntag v1\ntagger t <t@e> 1 +0000\n\nmessage\n";
        let sig = b"-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n";
        let mut tag = body.to_vec();
        tag.extend_from_slice(sig);
        let (payload, signature) = parse_signed_buffer(&tag).expect("should split");
        assert_eq!(payload, body);
        assert_eq!(signature, sig);
    }

    #[test]
    fn parse_signed_buffer_none_when_unsigned() {
        let body = b"object 0123\ntype commit\ntag v1\ntagger t <t@e> 1 +0000\n\nmessage\n";
        assert!(parse_signed_buffer(body).is_none());
    }

    #[test]
    fn parse_goodsig_and_trust() {
        let status = "\
[GNUPG:] NEWSIG\n\
[GNUPG:] GOODSIG 73D758744BE721698EC54E8713D758744BE7216 C O Mitter <committer@example.com>\n\
[GNUPG:] VALIDSIG FINGERPRINT 2010-04-01 1270074988 0 4 0 17 2 00 PRIMARYFPR\n\
[GNUPG:] TRUST_ULTIMATE 0 pgp\n";
        let mut sigc = SignatureCheck::default_none();
        parse_gpg_output(&mut sigc, status);
        assert_eq!(sigc.result, 'G');
        assert_eq!(sigc.trust_level, TrustLevel::Ultimate);
        assert_eq!(
            sigc.signer.as_deref(),
            Some("C O Mitter <committer@example.com>")
        );
        assert_eq!(
            sigc.key.as_deref(),
            Some("73D758744BE721698EC54E8713D758744BE7216")
        );
        assert!(sigc.verify_status(None));
        assert!(sigc.verify_status(Some(TrustLevel::Ultimate)));
        assert!(sigc.verify_status(Some(TrustLevel::Marginal)));
    }

    #[test]
    fn parse_badsig() {
        let status = "[GNUPG:] BADSIG KEYID Some Signer <s@example.com>\n";
        let mut sigc = SignatureCheck::default_none();
        parse_gpg_output(&mut sigc, status);
        assert_eq!(sigc.result, 'B');
        assert!(!sigc.is_good());
    }

    #[test]
    fn double_exclusive_status_is_error() {
        let status = "[GNUPG:] GOODSIG K1 A <a@x>\n[GNUPG:] BADSIG K2 B <b@x>\n";
        let mut sigc = SignatureCheck::default_none();
        parse_gpg_output(&mut sigc, status);
        assert_eq!(sigc.result, 'E');
    }

    #[test]
    fn min_trust_level_from_config_is_case_insensitive() {
        assert_eq!(
            TrustLevel::from_config("marginal"),
            Some(TrustLevel::Marginal)
        );
        assert_eq!(TrustLevel::from_config("FULLY"), Some(TrustLevel::Fully));
        assert_eq!(TrustLevel::from_config("bogus"), None);
    }

    #[test]
    fn format_detected_from_signature_armor() {
        assert_eq!(
            GpgFormat::from_signature(b"-----BEGIN SSH SIGNATURE-----\nABC\n"),
            Some(GpgFormat::Ssh)
        );
        assert_eq!(
            GpgFormat::from_signature(b"-----BEGIN PGP SIGNATURE-----\n"),
            Some(GpgFormat::OpenPgp)
        );
        assert_eq!(
            GpgFormat::from_signature(b"-----BEGIN PGP MESSAGE-----\n"),
            Some(GpgFormat::OpenPgp)
        );
        assert_eq!(
            GpgFormat::from_signature(b"-----BEGIN SIGNED MESSAGE-----\n"),
            Some(GpgFormat::X509)
        );
        assert_eq!(GpgFormat::from_signature(b"garbage"), None);
    }

    #[test]
    fn literal_ssh_key_detection() {
        assert_eq!(
            is_literal_ssh_key("key::ssh-ed25519 AAAA"),
            Some("ssh-ed25519 AAAA")
        );
        assert_eq!(
            is_literal_ssh_key("ssh-ed25519 AAAA"),
            Some("ssh-ed25519 AAAA")
        );
        assert_eq!(is_literal_ssh_key("/home/u/.ssh/id_ed25519"), None);
    }

    #[test]
    fn parse_ssh_output_trusted_principal() {
        let mut sigc = SignatureCheck::default_none();
        sigc.output =
            "Good \"git\" signature for principal with number 1 with ED25519 key SHA256:ABC\n"
                .to_owned();
        parse_ssh_output(&mut sigc);
        assert_eq!(sigc.result, 'G');
        assert_eq!(sigc.trust_level, TrustLevel::Fully);
        assert_eq!(sigc.signer.as_deref(), Some("principal with number 1"));
        assert_eq!(sigc.key.as_deref(), Some("SHA256:ABC"));
        assert_eq!(sigc.fingerprint.as_deref(), Some("SHA256:ABC"));
        assert!(sigc.primary_key_fingerprint.is_none());
    }

    #[test]
    fn parse_ssh_output_untrusted_unknown_key() {
        let mut sigc = SignatureCheck::default_none();
        // The trailing `No principal matched.` is appended on its own line by
        // stripspace; only the first line should be parsed.
        sigc.output = "Good \"git\" signature with ED25519 key SHA256:XYZ\nNo principal matched.\n"
            .to_owned();
        parse_ssh_output(&mut sigc);
        assert_eq!(sigc.result, 'G');
        assert_eq!(sigc.trust_level, TrustLevel::Undefined);
        assert!(sigc.signer.is_none());
        assert_eq!(sigc.key.as_deref(), Some("SHA256:XYZ"));
        assert_eq!(sigc.fingerprint.as_deref(), Some("SHA256:XYZ"));
    }

    #[test]
    fn parse_ssh_output_bad_signature() {
        let mut sigc = SignatureCheck::default_none();
        sigc.output = "Signature verification failed: incorrect signature\n".to_owned();
        parse_ssh_output(&mut sigc);
        assert_eq!(sigc.result, 'B');
        assert_eq!(sigc.trust_level, TrustLevel::Never);
        assert!(sigc.key.is_none());
    }

    #[test]
    fn stripspace_keeps_line_terminators() {
        // Each non-empty line keeps a trailing newline; trailing blanks dropped.
        assert_eq!(stripspace("Good ... SHA256:FPR\n"), "Good ... SHA256:FPR\n");
        assert_eq!(stripspace("a  \n\n\nb\n\n"), "a\n\nb\n");
        assert_eq!(stripspace(""), "");
        // Concatenating a stripspaced line with following stderr keeps them on
        // separate lines.
        let mut out = stripspace("Good ... SHA256:FPR\n");
        out.push_str("No principal matched.\n");
        assert_eq!(out.lines().next(), Some("Good ... SHA256:FPR"));
    }

    #[test]
    fn payload_committer_timestamp_parsed() {
        let payload =
            b"tree 0123\nauthor A <a@x> 1112912173 -0700\ncommitter C <c@x> 1112912273 +0200\n\nmsg\n";
        assert_eq!(payload_committer_timestamp(payload), Some(1112912273));
        // Falls back to tagger for tag payloads.
        let tag = b"object 0123\ntype commit\ntag v1\ntagger T <t@x> 1112912000 -0500\n\nmsg\n";
        assert_eq!(payload_committer_timestamp(tag), Some(1112912000));
        // No ident header.
        assert_eq!(payload_committer_timestamp(b"tree 0123\n\nmsg\n"), None);
    }
}
