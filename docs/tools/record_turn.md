# `record_turn`

> No longer in the tool list: `record_session` with one turn does the same. The server still answers it for clients that call it by name.

Appends one user/assistant exchange to a session in the local chat ledger. Call it only when the user asks to save or for a decision worth keeping, not after every response. Messages that contain secret signatures are rejected and nothing is saved; empty messages are skipped.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `session_id` | string | yes | - |
| `user_message` | string | no | empty |
| `assistant_message` | string | no | empty |
| `project_root` | string | no | auto-detected (see below) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "session_id": "auth-refactor",
  "user_message": "Why does search skip some files?",
  "assistant_message": "Files with secret-like names or content are never indexed; they are skipped until they change."
}
```

Reply:

```text
Saved 2 message(s) to session 'auth-refactor'.
```

Session ids are reduced to `[alphanumeric - _ .]`; if that changes the id, a short digest is appended so distinct ids never collide. Ledger: `<storage>/chats/<project digest>/<session>.json`.

[Back to all tools](../TOOLS.md)
