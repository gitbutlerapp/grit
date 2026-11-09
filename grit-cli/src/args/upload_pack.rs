//! Clap arguments for `grit upload-pack`.

#[derive(Debug, clap::Args)]
pub(crate) struct UploadPackArgs {
    #[command(flatten)]
    pub(crate) inner: super::ServeArgs,
}
