# `save_search_results`

Runs a semantic search (same as `search_context`) and saves the reply text under a name for the project. Use `load_saved_results` to read it back.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `query` | string | yes | - |
| `filename` | string | yes | - |
| `project_root` | string | no | auto-detected (see below) |
| `top_k` | integer | no | 5 |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "query": "secret file protection",
  "filename": "secrets",
  "top_k": 1
}
```

Reply:

```text
Saved to ~/.context-broker/results/<project digest>/secrets.json
```

The name is reduced to a file name and `.json` is added. File content: `{project_root, query, saved_at, results}`. Stored under `CONTEXT_BROKER_STORAGE_DIR` (default `~/.context-broker`).

[Back to all tools](../TOOLS.md)
