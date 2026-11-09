//! Clap arguments for `grit receive-pack`.

#[derive(Debug, clap::Args)]
pub(crate) struct ReceivePackArgs {
    #[command(flatten)]
    pub(crate) inner: super::ServeArgs,
}
