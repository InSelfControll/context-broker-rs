# Architecture

Context Broker (Rust) is a single-process MCP server over stdio. This document describes how it is put together. Every statement refers to the code in `src/`.

## Module map

| Module | Responsibility |
|--------|----------------|
| `main.rs` | The MCP loop (hand-written JSON-RPC 2.0, one message per line), the tool definitions (12, plus `query_graph` when a Graphify graph is available), dispatch, the idle-release thread |
| `config.rs` | Environment settings and the fixed policy tables (secret patterns, ignored directories, extensions, project markers, size limits) |
| `security.rs` | Secret detection: filename globs and content signatures |
| `project.rs` | Project root resolution, file collection (ignore-aware), safe file reads |
| `embed.rs` | `all-MiniLM-L6-v2` on `candle`: model lookup/download, tokenization, batch-parallel embedding |
| `index.rs` | The incremental index, its disk cache, cosine-similarity search and reply rendering |
| `snippet.rs` | Query-focused line selection within a token budget |
| `literal.rs` | Literal and regex search over the safe file set |
| `memory.rs` | Chat memory, history lookup and saved search results over three stores: JSON, Obsidian vault, Graphify memory docs |
| `graph.rs` | Read-only queries over a Graphify `graph.json`: node lookup, connections, shortest path |

```mermaid
flowchart TB
    main["main.rs<br/>MCP loop, dispatch, idle release"]
    index["index.rs<br/>index + search + render"]
    literal["literal.rs"]
    memory["memory.rs"]
    graph["graph.rs"]
    embed["embed.rs"]
    snippet["snippet.rs"]
    project["project.rs"]
    security["security.rs"]
    config["config.rs"]

    main --> index
    main --> literal
    main --> memory
    main --> graph
    main --> project
    index --> embed
    index --> snippet
    index --> project
    literal --> project
    memory --> security
    project --> security
    security --> config
    project --> config
    embed --> config
    index --> config
    memory --> config
    graph --> config
```

## Request flow

The process reads stdin line by line. Each line is one JSON-RPC message. A line that is not valid JSON gets a `-32700` error with `id: null`. A message without an `id` is a notification and is never answered. Output is one JSON line per request, flushed immediately; if writing fails (the client went away) the loop ends and the process exits. Closing stdin ends it the same way.

```mermaid
sequenceDiagram
    participant C as MCP client
    participant M as main.rs
    participant T as Tool (index / literal / memory)
    C->>M: {"method":"initialize", ...}
    M-->>C: protocol version, capabilities {tools}, serverInfo
    C->>M: {"method":"tools/list"}
    M-->>C: 12 tool definitions
    C->>M: {"method":"tools/call","params":{"name":"search_context", ...}}
    M->>M: lock App, resolve project_root
    M->>T: run tool
    T-->>M: reply text (or error string)
    M->>M: history note for search_context / find_in_codebase
    M-->>C: {"content":[{"type":"text",...}, ...],"isError":false}
```

- `initialize` echoes the requested protocol version when it is one of `2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`, and otherwise answers `2025-06-18`.
- `ping` returns `{}`; unknown methods return `-32601`.
- `tools/call` takes the `App` mutex, runs the tool and records the time of use (for idle release). A tool error is returned as a normal result with `isError: true` and the text `Error: <message>`.
- The reply is a list of text content blocks. Normally there is one; `search_context` and `find_in_codebase` append a second block with a short "related past work" note when the project's chat history strongly matches the question. There is no `structuredContent`.
- Progress and diagnostics go to stderr, never stdout.

## Indexing pipeline

The index holds one embedding per file. It is refreshed at the start of every search.

```mermaid
flowchart TD
    A["collect_files(root)"] --> B["signature per file<br/>[mtime_ns, size]"]
    B --> C{"fingerprint equals<br/>in-memory index?"}
    C -- yes --> S["search"]
    C -- no --> D{"fingerprint equals<br/>disk cache?"}
    D -- yes --> L["load cache into memory"] --> S
    D -- no --> E["for each file"]
    E --> F{"signature unchanged<br/>since cached row?"}
    F -- yes --> R["reuse cached vector"]
    F -- no --> G{"previously skipped<br/>with same signature?"}
    G -- yes --> K["stay skipped"]
    G -- no --> H["read_safe: secret filter"]
    H -- blocked/unreadable --> K
    H -- ok --> P["queue 'File: path / Content: head'"]
    P --> M["embed queued texts<br/>(batches of 8, worker threads)"]
    R --> N["new index"]
    M --> N
    N --> W["write index.bin + index.json<br/>atomically"] --> S
```

1. **Collect.** `project::collect_files` walks the root with the `ignore` crate: `.gitignore` and `.ignore` are honored, hidden files are visited, symlinks are not followed, ignored directory names (`node_modules`, `target`, `.git`, `.cache`, `.context-broker`, ...) are pruned, and only supported extensions under the size limit and without a secret-looking name are kept. The result is sorted.
2. **Signature and fingerprint.** Each file gets `[mtime_ns, size]`. A SHA-256 over all paths and signatures is the index fingerprint. An identical fingerprint means nothing changed: no file is read.
3. **Reuse or embed.** When the fingerprint differs, rows whose signature is unchanged are copied from the previous index (memory first, then disk). New or changed files are read with `read_safe` (at most 40,000 characters, secret filter applied). The first 3,000 characters become the text `File: <path>\nContent: <head>`. Files that are unreadable or secret-bearing are recorded in a `skipped` map and are not retried until their signature changes.
4. **Embed.** Queued texts are split into batches of 8 and processed by up to `CONTEXT_BROKER_EMBED_THREADS` worker threads that pull batches from a shared counter. Each batch is tokenized (max 256 tokens, padded), run through the BERT model, mean-pooled over real tokens and L2 normalized.
5. **Persist.** Vectors are stored as little-endian `f32` in `index.bin`; paths, signatures, skipped files, model name, dimension, cache version and fingerprint in `index.json`. Both live in `<project>/.cache/context-broker-rs/` and are written to a temporary file and renamed. A cache with a different version, model or size is ignored.

## Search and snippet selection

1. The query is embedded with the same model. Every index row is scored by dot product (vectors are normalized, so this is cosine similarity), sorted descending, and the top `top_k` (1 to 20) rows are taken.
2. For each, the file is re-read through `read_safe` (a file that has become secret-bearing yields no hit) and `snippet::focused` selects lines:
   - query terms are lower-cased words of at least 3 characters, minus a small stopword list, at most 16;
   - a term that is rare in the file weighs more than one on every line (weight is `1 / lines containing it`);
   - lines that look like definitions (`fn`, `def`, `class`, `struct`, ...) score double;
   - best lines are taken first, each with `CONTEXT_BROKER_RESULT_SNIPPET_CONTEXT_LINES` neighbours, until `CONTEXT_BROKER_RESULT_MAX_TOKENS_PER_FILE` (estimated at about 3.7 characters per token) is spent;
   - gaps in the selection are marked with `@L<line>`; a file with no query term falls back to its head.
3. `index::render` prints `<shown> of <indexed> files · root: <root>` once, then `### <relative path>` and the snippet for each hit.

`find_in_codebase` is separate: it escapes the pattern unless `use_regex` is set, compiles it with the `regex` crate (linear time, 1 MiB size limit), and scans the same collected files through `read_safe`.

## Security layers

Policy tables are constants in `config.rs`; no setting can relax them.

```mermaid
flowchart LR
    F["file path"] --> L1{"1. filename / path<br/>matches secret glob?"}
    L1 -- yes --> X["never read"]
    L1 -- no --> L2["read up to 40,000 chars"]
    L2 --> L3{"2. content signature<br/>on any line?"}
    L3 -- yes --> X
    L3 -- no --> OK["indexed / searched / snippeted"]
```

1. **Path layer.** `security::is_secret_path` matches the file name and the relative path against 35 globs (`.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa`, `.aws/credentials`, `*.tfstate`, `secrets.*`, `*.token`, ...). It runs during collection and again inside every read.
2. **Content layer.** `security::has_secret_content` scans the lines that were read. Comment lines starting with `#` are ignored. A line is a hit if it contains a value signature (private key headers, `_authToken`, `_auth`, `Bearer `) or starts with an assignment (`KEY=` or `key:`, optional `export` and quotes) whose name contains a key signature such as `API_KEY`, `PASSWORD`, `SECRET`, `TOKEN` or `KEY`. The assignment anchor keeps ordinary code such as `token = make_token()` indexable.
3. **Memory layer.** `memory::record` refuses messages with secret content (nothing is saved), and history lookups skip exchanges that have it.

All file reads go through `project::read_safe`, so indexing, snippets and literal search share the same rules. Walking never follows symlinks.

## Memory and storage layout

```mermaid
flowchart LR
    subgraph Global["CONTEXT_BROKER_STORAGE_DIR (default ~/.context-broker)"]
        chats["chats/&lt;digest&gt;/&lt;session&gt;.json"]
        results["results/&lt;digest&gt;/&lt;name&gt;.json"]
    end
    subgraph Project["&lt;project&gt;/"]
        cache[".cache/context-broker-rs/<br/>index.json, index.bin"]
        local[".context-broker/chats/&lt;digest&gt;/ (read only)"]
    end
    record_turn --> chats
    record_session --> chats
    save_search_results --> results
    load_chat_context --> chats
    load_cross_session_context --> chats
    lookup_project_history --> chats
    load_chat_context --> local
```

- **Digest.** The first 16 hex characters of the SHA-256 of the canonical project root path.
- **Chat ledger.** One JSON file per session: `{project: {digest, name, root}, session_id, messages: [{peer_id, content, created_at}], updated_at}`. Writes are append-only: read, extend, write to a temporary file, rename. A `.lock` file (created exclusively, stale after 10 s) serializes writers across processes. Session ids are reduced to `[alphanumeric - _ .]`; a lossy reduction appends a short digest so ids never collide.
- **History.** Lookups read the 128 most recently modified ledgers (each up to 1 MB) from the global store and from `<project>/.context-broker/chats/<digest>/`. `lookup_project_history` and the automatic note use term overlap: at least 2 shared terms and at least 60% of the query's terms; an exchange is a question plus the assistant message that follows it when both fit in 2,000 characters.
- **Saved results.** `<storage>/results/<digest>/<name>.json` with `{project_root, query, saved_at, results}`.

### Recall by meaning

`lookup_project_history` first takes the term-overlap matches, then adds exchanges that mean the same as the query in other words (`memory::semantic_matches`). The 300 newest exchanges are embedded with the search model and compared with the query by cosine similarity; those at 0.35 or above count, best first, up to three results in total. Each exchange is embedded once: vectors are kept in `<storage>/recall/<project digest>.bin`, keyed by the SHA-256 of the exchange text, for every store, and the file is rewritten without exchanges that no longer exist. A message directly answered by the assistant forms one exchange with its answer and is not compared a second time on its own. If the model cannot be loaded, the term-overlap matches are returned alone. The automatic note on search replies stays term-overlap only, so it adds no model work.

### Other stores

`CONTEXT_BROKER_STORAGE` (`config::store`) selects where `memory.rs` reads and writes. The JSON layout above is the default. The public functions are the same for every store; each store supplies only how a session is written and how sessions are read back as `(peer, content)` messages.

- **Obsidian** (`obsidian`, or just `CONTEXT_BROKER_OBSIDIAN_VAULT`). `<vault>/<folder>/<project name>-<first 8 digest characters>/Chats/<session>.md`, one note per session with YAML front matter (`type`, `project`, `root`, `session`, `created`, `tags`). A message is a callout: a `> [!quote] user · <UTC time>` or `> [!info] assistant · <UTC time>` line followed by the content, every line prefixed with `>`. Because all content lines carry the prefix, a callout header is only recognized on the first quoted line after an unquoted one, so message text cannot start a new message. Appends use the same lock-and-rename as the JSON ledger. Saved results are `Results/<name>.md`.
- **Graphify** (`graphify`). `<project>/graphify-out/memory/chat_<UTC stamp>_<microseconds><turn>_<slug>.md`, one doc per exchange, in the layout written by `graphify save-result`: front matter (`type: "chat"`, `date`, `question`, `contributor: "context-broker"`, `session`), then `# Q: <user>` and `## Answer` with the assistant message. File names sort in conversation order. Docs without a `session` (written by Graphify) are read as the session `graphify`, minus their `## Outcome` and `## Source Nodes` sections. Up to 512 docs are read. Saved results are `result_<name>.md` in the same folder and are not treated as chat history.

In both Markdown stores, messages with secret signatures are dropped when read, since the files can be edited outside the broker.

### Obsidian layouts

`config::obsidian_ai_chats` decides how notes are laid out in a vault: the vault's own AI-chat convention when `<vault>/AI Chats/Chats` exists (or `CONTEXT_BROKER_OBSIDIAN_LAYOUT=ai-chats`), otherwise the callout layout above, whose folder is placed under Obsidian's configured folder for new notes (`newFileLocation: "folder"` in `.obsidian/app.json`).

In the AI-chat layout a session is `AI Chats/Chats/Context Broker/Context Broker <date> - <session> - <id>.md`, where the id is the first 8 hex characters of the SHA-256 of `<project digest>/<session>`; the note is found again by that ending. Front matter: `type: ai-chat`, `platform`, `source`, `conversation_id`, `title`, `session`, `project`, `project_root`, `created`, `updated`, `message_count`, `tags`. The body has `Source:`, `Projects:` and `Messages:` lines, then `## Conversation` with one `### NN. User|Assistant — <UTC time>` section per message. Appending adds sections and rewrites only the `updated`, `message_count` and `Messages` lines. A content line that looks like a message heading is written with a leading backslash, removed again on reading, so text cannot start a new message. A new chat is added once to `AI Chats/Sources/AI Source - Context Broker.md` and `AI Chats/Projects/AI Project - <project>.md`, which are created when missing. All projects share the folder, so reading keeps the notes whose `project_root` is the project. Only the `Context Broker` platform folder is read or written. Saved results are `AI Chats/Results/<project>-<digest>/<name>.md`.

### Detection and `auto`

`config::resolve_store` turns a store name into a store. `obsidian` uses `CONTEXT_BROKER_OBSIDIAN_VAULT` or, without it, `config::detected_vault`: the `vaults` map of Obsidian's `obsidian.json` (looked up under `XDG_CONFIG_HOME`, `APPDATA`, `~/.config`, the Flatpak and Snap directories and `~/Library/Application Support`, or the file named by `CONTEXT_BROKER_OBSIDIAN_CONFIG`), keeping vaults whose folder exists and taking the open one, then the most recently used. `auto` takes a vault if there is one, then Graphify if `config::graphify_detected` (the project has `graphify-out/`, or a `graphify` executable is on `PATH`), then JSON. Detection runs once per process. A configured vault that does not exist is an error, never a silent fallback.

### Export

`memory::export` (the `export_memory` tool) copies the JSON store into Obsidian or Graphify; with no target it uses `auto` and fails if that finds neither. For each ledger it pairs a user message with the assistant message right after it and hands the exchanges, with their original `created_at` times, to the same writer that live recording uses. It is repeatable. For Obsidian, messages the note already holds (compared as they read back from the note) are blanked before writing. For Graphify, a doc's name is fixed by time, position and text, and existing names are skipped. Saved results are written as Markdown docs unless a file of that name exists. With `all_projects`, every digest folder under the storage directory is exported to the project root its files record; folders whose project no longer exists are counted and skipped.

## Graphify graph queries

`graph.rs` reads `<project>/graphify-out/graph.json` (node-link JSON: `nodes` with `id`, `label`, `source_file`, `source_location`, `community`; `links` or `edges` with `source`, `target`, `relation`, `confidence`). Only those fields are deserialized. Edges with an endpoint outside the node set are dropped. A parsed graph is kept per project root until the file's modification time changes, and is released with the other idle resources.

`query_graph` resolves a name in this order: exact id; exact label ignoring case (the most connected node wins when several share it); label substring; then, for a question rather than a name, nodes ranked by how many of its words their label carries. One match is used; several are returned as a list of ids to choose from. With `to`, a breadth-first search over undirected adjacency gives the shortest chain, printed with each edge's real direction. `EXTRACTED` edges are printed without a tag; any other confidence is shown.

## HTTP transport

With `CONTEXT_BROKER_HTTP_ADDR` set, `main.rs` serves `POST /mcp` with `tiny_http` instead of reading stdin: one JSON-RPC message per request, answered as `application/json` (notifications get `202`), other methods `405`, other paths `404`, bodies capped at 1 MB. Requests are handled one at a time through the same `respond` function as stdio. `project::lock_root` confines every call to `CONTEXT_BROKER_PROJECT_ROOT`: an explicit `project_root` must resolve inside it. In this mode the tool list gains `read_file`, which resolves a relative path inside the root (`project::inside` refuses `..`, absolute paths and symlinks that leave it) and returns numbered lines through `project::read_safe`. The transport has no authentication; it is meant to listen on localhost behind an authenticating proxy.

## Threading and idle release

- **Main thread** reads stdin and answers requests one at a time. All engine state (the model, the per-project indexes) sits behind one `Mutex<App>`.
- **Embedding workers.** Only during bulk embedding: up to `CONTEXT_BROKER_EMBED_THREADS` scoped threads, each pulling whole batches. Parallelism is across batches, so `RAYON_NUM_THREADS` is set to 1 at startup (unless already set) to stop the matrix library from oversubscribing the machine.
- **Idle thread.** Wakes every `min(timeout, 30)` seconds. If no tool call has happened for `CONTEXT_BROKER_IDLE_RESOURCE_TIMEOUT_SECONDS` (default 900; 0 disables the thread) and the lock is free, it drops the model and all in-memory indexes and, on glibc Linux, calls `malloc_trim`. The next search reloads the index from the disk cache, which is cheap. Large embedding runs also trigger `malloc_trim` when they finish.
- **Model loading** is lazy: the first call that needs an embedding loads it (memory-mapped weights), which is why startup is a couple of milliseconds.

## Differences from the Python architecture

| | Python Context Broker | This port |
|---|---|---|
| Runtime | Python 3.13, FastMCP, PyTorch, scikit-learn | Rust, `candle`, hand-written JSON-RPC |
| Tool surface | 36 tools, router, handoffs, delegation, generators | 12 tools: search, memory, saved results, export (plus `query_graph` with a Graphify graph) |
| Transports | stdio, SSE, streamable HTTP, shared service | stdio only |
| Embedding | sentence-transformers (in a worker process) | the same weights, in process via `candle` |
| Index | cached JSON | `index.json` + raw `f32` `index.bin` |
| Cross-chat memory | Honcho or Redis backends, dashboard | local JSON ledger only |
| Replies | JSON/text with reports | plain text blocks, root stated once, no telemetry |
| Shared layout | ledgers under `<storage>/chats/<digest>/` | same layout and schema |

Not ported: Honcho and Redis backends, the web dashboard, task delegation, model handoffs, the AGENTS.md / changelog / feature-docs generators, the environment doctor, the plan-execution tools, and the SSE / HTTP transports.
