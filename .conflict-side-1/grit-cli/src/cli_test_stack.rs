//! Run work that builds the full clap command tree on a larger stack (debug unit tests).

/// Stack size for [`run`] — deep enough for `Cli::command()` in debug builds.
pub const STACK: usize = 16 * 1024 * 1024;

/// Run `f` on a dedicated thread with [`STACK`].
pub fn run<R, F>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(f)
        .expect("spawn CLI tree test thread")
        .join()
        .expect("CLI tree test thread")
}
