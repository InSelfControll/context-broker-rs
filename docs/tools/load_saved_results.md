# `load_saved_results`

Loads a saved search result: the original query on the first line, then the saved reply.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `filename` | string | no | none (lists the saved results, one name per line) |
| `project_root` | string | no | auto-detected (see below) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "filename": "secrets"
}
```

Reply:

```text
query: secret file protection
1 of 9 files · root: /path/to/project
### src/security.rs
//! Secret protection: filename patterns plus content signatures, applied to every read.
@L30
/// True when the file name or relative path matches a hard-coded secret pattern.
```

Missing file: error `saved result not found: <filename>`.

[Back to all tools](../TOOLS.md)
