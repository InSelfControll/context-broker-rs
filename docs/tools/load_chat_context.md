# `load_chat_context`

Loads a stored session, newest messages last, as compact text. `search_query` keeps only messages containing that text (case-insensitive).

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `session_id` | string | yes | - |
| `project_root` | string | no | auto-detected (see below) |
| `search_query` | string | no | none |
| `limit` | integer | no | 20 |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "session_id": "cache-design",
  "limit": 3
}
```

Reply:

```text
session cache-design · 3 of 4 messages
[assistant] Vectors in index.bin, file signatures in index.json under .cache/context-broker-rs.
[user] What triggers re-embedding?
[assistant] A changed mtime or size.
```

Unknown session: `No stored messages for session '<id>'.`

[Back to all tools](../TOOLS.md)
