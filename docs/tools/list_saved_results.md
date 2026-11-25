# `list_saved_results`

> No longer in the tool list: `load_saved_results` without a `filename` does the same. The server still answers it for clients that call it by name.

Lists the saved search results of the project, one file name per line, sorted.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `project_root` | string | no | auto-detected (see below) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{}
```

Reply:

```text
secrets.json
```

No saved results: `No saved results.`

[Back to all tools](../TOOLS.md)
