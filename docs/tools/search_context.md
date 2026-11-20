# `search_context`

Semantic search. Embeds the query, ranks every indexable file by cosine similarity and returns, for the top files, only the lines that best match the query. The reply is plain text with the project root stated once, relative paths, and `@L<line>` markers where lines were skipped. Omit `query` for a project overview (it then searches for "main entry point configuration setup architecture"). The first call in a project builds the index; later calls re-embed only changed files. When the project history strongly matches the query, a second text block with a short "related past work" note is appended.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `query` | string | no | overview query |
| `project_root` | string | no | auto-detected (see below) |
| `top_k` | integer | no | 5 (clamped to 1-20) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "query": "how are secret files blocked from indexing"
}
```

Reply:

```text
5 of 9 files · root: /path/to/project
### src/security.rs
//! Secret protection: filename patterns plus content signatures, applied to every read.
@L30
/// True when the file name or relative path matches a hard-coded secret pattern.
pub fn is_secret_path(rel_path: &str) -> bool {
    let rel = rel_path.replace('\\', "/");
@L37
/// True when any line of `content` carries a secret signature.
pub fn has_secret_content(content: &str) -> bool {
### src/index.rs
@L34
    vectors: Vec<f32>,
    dim: usize,
    /// Unreadable or secret-bearing files, skipped again until they change.
(... three more files)
```

Trimmed to two of the five files. The header counts shown files out of indexed files. Lines are chosen per file within a token budget (`CONTEXT_BROKER_RESULT_MAX_TOKENS_PER_FILE`, default 240) with `CONTEXT_BROKER_RESULT_SNIPPET_CONTEXT_LINES` lines of context. Errors: empty project (`no indexable files found in ...`).

[Back to all tools](../TOOLS.md)
