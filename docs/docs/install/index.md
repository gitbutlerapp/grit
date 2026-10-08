# Install grit

> Install the grit CLI on your machine, try nightly builds, and keep your copy up to date.

## Stable release (recommended)

On macOS and Linux:

```bash
curl -fsSL https://grit-scm.com/install | sh
```

On Windows, run this in PowerShell:

```bash
irm https://grit-scm.com/install.ps1 | iex
```

The script downloads the latest release from GitHub, installs the `grit` binary (by default into `~/.local/bin` on Unix), and prints the version it installed.

## Nightly builds

CI publishes a rolling **nightly** prerelease when the release workflow is triggered manually. To install that build instead of the latest stable release:

```bash
curl -fsSL https://grit-scm.com/install-nightly | sh
```

On Windows:

```bash
irm https://grit-scm.com/install-nightly.ps1 | iex
```

Nightlies track recent `main`; use them when you want to test fixes before the next tagged release.

## Install with Cargo

If you already have a Rust toolchain:

```bash
cargo install grit-cli
```

This builds and installs the `grit` executable into Cargo's bin directory (usually `~/.cargo/bin`). Make sure that directory is on your `PATH`.

## Keep grit up to date

After a script install, run [`grit update`](https://grit-scm.com/docs/update/index.md) to re-run the same installer and replace the binary you're running. If you installed with Cargo, update with:

```bash
cargo install grit-cli
```

## Build from source

Clone [the repository](https://github.com/gitbutlerapp/grit) and build the CLI crate:

```bash
cargo build --release -p grit-cli
```

The binary is `target/release/grit`. You can copy it anywhere on your `PATH`, or run it from the build tree.

### Toolchain

The workspace pins a Rust version in [`rust-toolchain.toml`](https://github.com/gitbutlerapp/grit/blob/main/rust-toolchain.toml) (currently **1.99.0** with `rustfmt` and `clippy`). `rustup` picks this up automatically when you build in the repo.

To use `grit-lib` in your own crate, add it from [crates.io](https://crates.io/crates/grit-lib) — see the [library quick start](https://grit-scm.com/docs/library-quickstart/index.md).
