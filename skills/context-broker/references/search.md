# Searching with Context Broker

## `search_context`: search by meaning

Describe what the code does in a full phrase, the way you would ask a colleague: "how are secret files kept out of the index", not "secret". The query is compared with file contents by meaning, so words that never appear in the code still work.

- `query` (optional): leave it out for a project overview (entry point, configuration, architecture). Do this first in a project you have not seen.
- `top_k` (default 5, 1 to 20): raise it only when the first reply misses what you need.

Reading the reply:

```text
5 of 9 files · root: /path/to/project
### src/security.rs
//! Secret protection: filename patterns plus content signatures, applied to every read.
@L30
pub fn is_secret_path(rel_path: &str) -> bool {
```

- The header says how many files are shown out of how many are indexed, and states the root once. Paths under `###` are relative to it.
- `@L30` means lines were skipped and the next line shown is line 30. The reply holds the best-matching lines only, not the whole file, so open the file at that line when you need the surrounding code.
- A second block starting `Related past work` may follow. It comes from stored conversations. Treat it as a lead and check it against the current code, because the code may have changed since.

The first search in a project builds the index and can take a while on a large project. Later searches only re-read changed files.

Only text and source files are indexed (Markdown, JSON, TOML, YAML, Rust, Python, Go, TypeScript, JavaScript, Java, shell, SQL, HTML, CSS and a few more). Files ignored by `.gitignore`, very large files and anything that looks like a secret (`.env`, key files, lines such as `API_KEY=`) are never read. If the user asks about such a file, say the broker skips it by design; do not treat its absence as proof the file does not exist.

## `find_in_codebase`: search by exact text

Use it when you know the string: a function name, an error message, a config key.

- `pattern` (required, 1 to 500 characters).
- `use_regex` (default false): set it for a regular expression. Without it the pattern is matched literally, so `fn embed(` needs no escaping.
- `case_sensitive` (default false).
- `file_glob`: narrows by relative path or file name, for example `*.rs` or `src/*`.

The reply lists at most 20 files and 10 matches per file, with a notice when it was cut. When you hit the limit, narrow the pattern or add a `file_glob`; do not assume you saw everything.

A good default is to start with `search_context` to find the area, then use `find_in_codebase` for the exact names that reply shows you.

## `read_file`: remote brokers only

A remote broker lists this tool because you cannot open its files yourself. Pass `path` as shown in a search reply (relative to the root), with `start_line` (default 1) and `max_lines` (default 200). Use the `@L` markers from `search_context` to choose the start line. When the tool is not listed, the project is on your own machine: read the file with your normal file tool.

## `query_graph`: only when listed

Listed when the project has a Graphify graph (`graphify-out/graph.json`). The broker reads the graph and never builds it.

- `node` alone: that node and its connections. `-->` leaves the node, `<--` arrives at it.
- `node` and `to`: the shortest path between the two.

Both accept an id, a label, part of a label or a short question. If the reply says several nodes match, call again with one of the ids it lists. If it ends with `+N more (raise limit)`, raise `limit` (default 20).

Use it for "what calls this", "what depends on this" and "how does A reach B". Those are questions search answers poorly.

## `route_task`

Give it the task in a sentence and it names the tools that fit. It only matches words against tool descriptions, so use it when you are unsure, not as a required first step.

