# grit completions

> Generate shell completion scripts for bash, zsh, and fish.

## Synopsis

```text
grit completions <shell>
```

## Description

Prints a shell completion script for the `grit` subcommands and flags in the running binary. Redirect stdout to a file your shell loads, or evaluate it in the current session during setup.

Supported shells: `bash`, `elvish`, `fish`, `powershell`, and `zsh` (whatever `clap_complete` exposes for the installed `grit` version).

The script always matches the CLI you built or installed. After [`grit update`](https://grit-scm.com/docs/update/index.md), regenerate completions so new commands and flags appear.

Release archives also ship pre-generated scripts under `completions/` when you install from a tarball.

## Options

| Argument | Meaning |
| -------- | ------- |
| `<shell>` | One of `bash`, `elvish`, `fish`, `powershell`, or `zsh`. |

`grit completions` ignores the global [`--json`](https://grit-scm.com/docs/global-options/index.md) and [`--filter`](https://grit-scm.com/docs/global-options/index.md) flags; stdout is always the raw completion script.

## Examples

Generate bash completions for the current session:

```console
$ grit completions bash > /tmp/grit.bash
$ source /tmp/grit.bash
```

Install zsh completions for your user (typical layout):

```bash
grit completions zsh > ~/.local/share/zsh/site-functions/_grit
```

Install fish completions:

```bash
mkdir -p ~/.config/fish/completions
grit completions fish > ~/.config/fish/completions/grit.fish
```

## JSON output

There is no JSON form. `--json` does not change stdout; use the human completion script only.

## See also

[Install grit](https://grit-scm.com/docs/install/index.md), [grit update](https://grit-scm.com/docs/update/index.md), [Global options](https://grit-scm.com/docs/global-options/index.md)
