# `find_in_codebase`

Exact literal or regular-expression search over the same safe file set as the semantic index (secret files are never read). Returns file, line number and the trimmed line. Matching is case-insensitive unless `case_sensitive` is true. Regexes use Rust's linear-time engine, so no timeout is needed. Use it when you already know the symbol or string.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `pattern` | string | yes | - |
| `project_root` | string | no | auto-detected (see below) |
| `case_sensitive` | boolean | no | false |
| `use_regex` | boolean | no | false |
| `file_glob` | string | no | none (matches the relative path or the file name) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "pattern": "fn embed",
  "file_glob": "*.rs"
}
```

Reply:

```text
1 matches in 1/8 files · root: /path/to/project
### src/index.rs
L124: fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
```

Limits: pattern 1-500 characters; at most 20 files and 10 matches per file are listed (a truncation notice is added); lines are cut at 200 characters. With no match: `No matches for '<pattern>' in <n> files.` Like `search_context`, it can append a related-past-work note.

[Back to all tools](../TOOLS.md)
