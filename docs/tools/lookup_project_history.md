# `lookup_project_history`

Checks whether an issue was handled before. Compares the query with stored exchanges (a question is paired with the assistant message right after it) and returns up to three past exchanges that share at least two terms and at least 60% of the query's terms. Output is evidence only; verify against current code.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `query` | string | yes | - |
| `project_root` | string | no | auto-detected (see below) |

`project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.

## Example

Call (arguments shown; `project_root` omitted):

```json
{
  "query": "why does search skip secret files"
}
```

Reply:

```text
Related past work (evidence only; verify against current code):
--- auth-refactor.json
Why does search skip some files?
Files with secret-like names or content are never indexed; they are skipped until they change.
```

No match: `No related project history.` Exchanges over 2,000 characters or containing secret signatures are ignored.

Matching is by shared words first (at least 2 shared terms and 60% of the query), then by meaning: stored exchanges are embedded once with the search model and cached, so a question asked in different words ("how to roll back a release") still finds the earlier answer ("Redeploy the previous tag.").

[Back to all tools](../TOOLS.md)
