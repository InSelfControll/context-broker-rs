# `record_session`

Appends a whole conversation to a session in one call. Summarize long replies instead of repeating them. Same secret rejection and ledger as `record_turn`.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `session_id` | string | yes | - |
| `turns` | array of {user, assistant} | yes | - |
| `project_root` | string | no | auto-detected (see below) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "session_id": "cache-design",
  "turns": [
    {
      "user": "How is the index cached?",
      "assistant": "Vectors in index.bin, file signatures in index.json under .cache/context-broker-rs."
    },
    {
      "user": "What triggers re-embedding?",
      "assistant": "A changed mtime or size."
    }
  ]
}
```

Reply:

```text
Saved 4 message(s) to session 'cache-design'.
```

`turns` must be a list; anything else returns `turns must be a list of {user, assistant} objects`.

[Back to all tools](../TOOLS.md)
