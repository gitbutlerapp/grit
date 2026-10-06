//! `grit mailinfo` — extract patch from email message.

#![allow(clippy::field_reassign_with_default)]
use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::mailinfo::{apply_mailinfo_config, mailinfo, MailinfoOptions, QuotedCrAction};
use grit_lib::repo::Repository;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, ClapArgs)]
#[command(
    about = "Extract patch from a single email message",
    override_usage = "grit mailinfo [OPTIONS] <msg> <patch>"
)]
pub struct Args {
    #[arg(short = 'k', long)]
    pub keep: bool,

    #[arg(short = 'b', long)]
    pub keep_body: bool,

    /// Re-code metadata to `i18n.commitEncoding` (default UTF-8).
    #[arg(short = 'u')]
    pub reencode_metadata: bool,

    /// Do not re-encode metadata.
    #[arg(short = 'n', long = "no-reencode")]
    pub no_reencode: bool,

    /// Re-code metadata to this encoding.
    #[arg(long = "encoding", value_name = "ENCODING")]
    pub explicit_encoding: Option<String>,

    #[arg(short = 'm', long = "message-id")]
    pub message_id: bool,

    #[arg(long)]
    pub scissors: bool,

    #[arg(long = "no-scissors")]
    pub no_scissors: bool,

    #[arg(long = "no-inbody-headers")]
    pub no_inbody_headers: bool,

    #[arg(long = "quoted-cr")]
    pub quoted_cr: Option<String>,

    pub msg: PathBuf,

    pub patch: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    let mut opts = MailinfoOptions::default();
    opts.keep_subject = args.keep;
    opts.keep_non_patch_brackets_in_subject = args.keep_body;
    opts.add_message_id = args.message_id;

    if args.no_reencode {
        opts.metainfo_charset = None;
    } else if args.reencode_metadata {
        opts.metainfo_charset = Some("utf-8".to_string());
    }
    if let Some(enc) = args.explicit_encoding.as_ref() {
        opts.metainfo_charset = Some(enc.clone());
    }

    if args.scissors {
        opts.use_scissors = true;
    }
    if args.no_scissors {
        opts.use_scissors = false;
    }
    if args.no_inbody_headers {
        opts.use_inbody_headers = false;
    }

    if let Some(ref s) = args.quoted_cr {
        let a =
            QuotedCrAction::parse(s).with_context(|| format!("bad action for --quoted-cr: {s}"))?;
        opts.quoted_cr = a;
    }

    if let Ok(repo) = Repository::discover(None) {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        apply_mailinfo_config(&cfg, &mut opts);
    }

    if args.scissors {
        opts.use_scissors = true;
    }
    if args.no_scissors {
        opts.use_scissors = false;
    }
    if args.no_inbody_headers {
        opts.use_inbody_headers = false;
    }
    if let Some(ref s) = args.quoted_cr {
        opts.quoted_cr =
            QuotedCrAction::parse(s).with_context(|| format!("bad action for --quoted-cr: {s}"))?;
    }

    let mut stdin = io::stdin();
    let mut input = Vec::new();
    stdin.read_to_end(&mut input).context("reading stdin")?;

    let mut msg_file = File::create(&args.msg).context("creating msg file")?;
    let mut patch_file = File::create(&args.patch).context("creating patch file")?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stderr = io::stderr();
    let mut err = stderr.lock();

    mailinfo(
        &input,
        &opts,
        &mut msg_file,
        &mut patch_file,
        &mut out,
        &mut err,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(())
}
