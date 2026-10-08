# grit tag

> List tags, or create or delete one. A new tag points at the current commit.

## Synopsis

```text
grit tag
grit tag <name>
grit tag -d <name>
```

## Description

With no arguments, lists your tags. With a name, creates a tag pointing at the current commit; tags created by `grit` are lightweight tags, with no message of their own. With `-d`, deletes a tag.

Tags are local until you publish them with [`grit push --tags`](https://grit-scm.com/docs/push/index.md).

## Options

| Option | Description |
| --- | --- |
| `<name>` | The tag to create or delete. Omit to list tags. |
| `-d`, `--delete` | Delete the tag. |

## Examples

```console
$ grit tag v0.1
Created tag v0.1

$ grit tag
  v0.1

$ grit push --tags
  pushed --tags → origin refs/tags/v0.1

$ grit tag -d v0.1
Deleted tag v0.1
```

## JSON output

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, or `delete`. |
| `tags` | array | For `list`, each tag with `name` and `oid`. |
| `name` | string | For `create` or `delete`, the tag name. |
| `oid` | string | For `create`, full id of the commit the tag points at. |

Listing tags:

```json
{
  "action": "list",
  "tags": [
    {
      "name": "v0.1",
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf"
    }
  ]
}
```

Creating a tag:

```json
{
  "action": "create",
  "name": "v0.1",
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf"
}
```

## See also

[grit push](https://grit-scm.com/docs/push/index.md), [grit show](https://grit-scm.com/docs/show/index.md)
