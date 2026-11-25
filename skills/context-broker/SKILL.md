---
name: context-broker
description: How to use the Context Broker MCP server (context-broker-rs) - code search by meaning or exact text, project memory across sessions, saved results, export to Obsidian. Use whenever its tools (search_context, find_in_codebase, record_session, lookup_project_history, export_memory and the rest) are available and the user wants to find code, explore a codebase, recall an earlier chat or decision, or save or push this chat to Obsidian.
---

# Context Broker

An MCP server for search and memory in one project. Replies are short plain text: ask a focused question, then open only the lines that matter. Tool names may carry a client prefix (`mcp__context-broker__...`).

## Pick the tool

| Goal | Tool |
|---|---|
| Find code you can describe; no `query` gives a project overview | `search_context` |
| Find a known symbol or string (`use_regex`, `file_glob`) | `find_in_codebase` |
| Callers, dependencies, path from A to B (when listed) | `query_graph` |
| Read lines of a file on a remote broker (when listed) | `read_file` |
| Was this problem handled before? | `lookup_project_history` |
| Find an earlier message by a word or two | `load_cross_session_context` |
| Reopen one session by id | `load_chat_context` |
| Save a chat or a decision | `record_session` |
| Keep and reuse a search reply | `save_search_results`, `load_saved_results` (no `filename` lists them) |
| Copy stored history to Obsidian or Graphify | `export_memory` |

## Rules

- **Leave `project_root` out.** The server detects it. A remote broker refuses any path outside its folder: on `project_root must be inside ...`, call again without it.
- **Search by meaning first, exact text second.** Phrase `search_context` as a full question. In its reply `@L30` means lines were skipped and the next line is line 30; open the file there for more.
- **"Related past work" is a lead, not a fact.** Check it against the current code.
- **Secret-looking files are never read** (`.env`, keys, `API_KEY=` lines). Their absence from results proves nothing.
- **Record only when asked or when a decision is worth keeping**, not after every reply. Summarize your replies in a few specific sentences (names, paths, versions, commands); keep the user's words. Use a descriptive `session_id` such as `auth-refactor-2026-11-23`; the same id appends to the same session.
- **Never put secret values in a recording.** The whole message is rejected.
- **"No matches" and "No related project history." are answers.** Try other words once, then say so.

## Push this chat to Obsidian

1. `record_session` with `session_id` and `turns: [{"user": "...", "assistant": "..."}]`.
2. `export_memory` with `target: "obsidian"`, once. Never pass `all_projects` unless the user asks for every project (a remote broker refuses it).
3. `0 new message(s)` is success: the broker stores straight into the vault, so step 1 already wrote the note. Tell the user the session id.

## More detail

Read these only when you need them:

- `references/search.md`: every search parameter, reply format, limits, `query_graph` and `read_file`.
- `references/memory.md`: recall and recording in depth, a worked example, where notes land in the vault, saved results.
- `references/errors.md`: each error reply and what to do.
