# `route_task`

Suggests which of this server's tools fit a task by counting words shared with each tool's name and description; words such as `exact`, `grep`, `regex`, `where is` or `def ` boost `find_in_codebase`. Falls back to `search_context`. It needs no project and reads no files.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `task` | string | yes | - |
| `top_k` | integer | no | 3 (clamped to 1-5) |


## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "task": "where is the function that parses config exactly"
}
```

Reply:

```text
find_in_codebase — Exact literal or regex search; returns file, line and snippet. Use for a known symbol or string.
```

Reply lines have the form `<tool> — <description>`.

[Back to all tools](../TOOLS.md)
