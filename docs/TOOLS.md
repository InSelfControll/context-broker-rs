# Tools

The server lists 9 tools over MCP stdio, plus `query_graph` when a Graphify graph is available. Every reply is plain text sent once, as one or more text content blocks; errors come back with `isError: true` and a message starting `Error:`.

| Tool | What it does | Required parameters |
|------|--------------|---------------------|
| [`search_context`](tools/search_context.md) | Semantic search; best-matching lines of the top files, or a project overview. | none |
| [`find_in_codebase`](tools/find_in_codebase.md) | Exact literal or regex search with file, line and snippet. | `pattern` |
| [`record_session`](tools/record_session.md) | Append a whole conversation to a session. | `session_id`, `turns` |
| [`load_chat_context`](tools/load_chat_context.md) | Load one stored session, newest messages last. | `session_id` |
| [`load_cross_session_context`](tools/load_cross_session_context.md) | Find stored messages across all sessions of the project. | `search_query` |
| [`lookup_project_history`](tools/lookup_project_history.md) | Up to three past exchanges that match an issue. | `query` |
| [`save_search_results`](tools/save_search_results.md) | Run a semantic search and save the reply under a name. | `query`, `filename` |
| [`load_saved_results`](tools/load_saved_results.md) | Load a saved search result. | `filename` |
| [`export_memory`](tools/export_memory.md) | Copy the stored history and saved results into Obsidian or Graphify. | none |
| [`query_graph`](tools/query_graph.md) | Graphify knowledge graph: a node's connections, or the shortest path between two nodes. Listed only when a graph is available. | `node` |

Groups: search (`search_context`, `find_in_codebase`), project memory (`record_session`, `load_chat_context`, `load_cross_session_context`, `lookup_project_history`), saved results (`save_search_results`, `load_saved_results`), export (`export_memory`), knowledge graph (`query_graph`).

The memory and saved-results tools write to the store selected by `CONTEXT_BROKER_STORAGE` (`json`, `obsidian`, `graphify` or `auto`); their parameters and replies are the same for all three. Saved results are named `<name>.json` in the JSON store and `<name>.md` in the other two.

**Answered but not listed.** [`route_task`](tools/route_task.md), [`record_turn`](tools/record_turn.md) and [`list_saved_results`](tools/list_saved_results.md) still work for clients that call them by name. They are left out of the tool list because every session pays for each listed tool: `record_session` takes one exchange or many, and `load_saved_results` without a `filename` lists the saved results.

The schemas carry no defaults and describe `project_root` once, on `search_context`; the server applies the defaults given on each tool's page.

All tools except `route_task` take an optional `project_root`. `project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory (strongest of `.git`, `Cargo.toml`, `package.json`, ... among the parents), then to the working directory.
