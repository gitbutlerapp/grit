---
title: Scripting with grit
summary: Use --json and --filter to build scripts on top of grit.
---

With `--json`, a command prints exactly one JSON value on stdout, so you can pipe it straight into other tools. The shape of each command's output is documented on its page. `--filter` runs a [jq](https://jqlang.org) expression over that value without needing `jq` installed. When a filter produces several values, they come back as one JSON array:

```
$ grit status --json --filter '{branch, clean}'
{
  "branch": "main",
  "clean": true
}

$ grit log --json --filter '.commits[].subject'
[
  "Say hi",
  "Start the project"
]
```

`grit` exits with status `0` on success and `1` when a command fails (including a push with a rejected ref). A mistake on the command line, such as an unknown option, exits with `2`.

Color is used only when stdout is a terminal. Set `NO_COLOR` to turn it off.
