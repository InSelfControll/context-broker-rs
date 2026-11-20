# `load_cross_session_context`

Finds stored messages that contain `search_query` (case-insensitive substring) across all sessions of the project, newest sessions first. Looks at the 128 most recently modified ledgers (files up to 1 MB) from the global storage and from `<project>/.context-broker/chats/`.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `search_query` | string | yes | - |
| `project_root` | string | no | auto-detected (see below) |
| `top_k` | integer | no | 10 (clamped to 1-50) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "search_query": "secret",
  "top_k": 5
}
```

Reply:

```text
1 match(es), newest sessions first
[auth-refactor/assistant] Files with secret-like names or content are never indexed; they are skipped until they change.
```

Each message is whitespace-flattened and clipped to 600 characters. No match: `No stored messages match '<query>'.`

[Back to all tools](../TOOLS.md)
