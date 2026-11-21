<div align="center">

# 🦀 Context Broker (Rust)

**Semantic code search and project memory for AI coding agents, in one 8 MB binary.**

A Rust port of the core of [Context Broker](https://github.com/InSelfControll/context-broker-mcp). It speaks the Model Context Protocol over stdio, finds relevant code by meaning, remembers decisions across sessions, and keeps every reply small: 12 tools, plain-text answers, no telemetry. 100% local, no API keys, no Python, no PyTorch.

[![Rust](https://img.shields.io/badge/Rust-2024%20edition-000000?style=for-the-badge&logo=rust&logoColor=white)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=for-the-badge)](LICENSE)
[![MCP](https://img.shields.io/badge/Protocol-MCP-green?style=for-the-badge)](https://modelcontextprotocol.io)
[![Transport](https://img.shields.io/badge/Transport-stdio-lightgrey?style=for-the-badge)](#-quick-start)

<img src="docs/assets/demo.gif" alt="An agent calls search_context and find_in_codebase and receives short plain-text replies" width="900"/>

**Works with:** Claude Code · Claude Desktop · Cursor · Codex CLI · any MCP client that can launch a stdio server

[🚀 Quick Start](#-quick-start) · [✨ Features](#-features) · [🧰 Tools](#-the-11-tools) · [📊 Performance](#-performance) · [⚙️ Settings](#%EF%B8%8F-settings) · [🏗️ Architecture](ARCHITECTURE.md)

</div>

---

## 🎯 Why a Rust port?

Every MCP tool you register costs tokens in every session, before the agent has done anything: the tool list is sent with each conversation. The Python Context Broker is already token-optimized, but it exposes 36 tools, needs a Python environment with PyTorch, and takes seconds to wake up. This port keeps the part agents use most (semantic search, literal search, chat memory, saved results) and makes it small and fast.

| | Python Context Broker | This port |
|---|---|---|
| Tool list each session pays for | 36 tools, 3,986 tokens | **12 tools, 1,191 tokens** |
| Average search reply (6 queries) | ~900 tokens | **~660 tokens** |
| Startup | 0.8 s | **2 ms** |
| Install | Python env + PyTorch | **one 8 MB binary** |
| Embedding model | `all-MiniLM-L6-v2` | the same weights, run with `candle` |

Numbers were measured on a 235-file repository against the already-optimized Python server; the [full table](#-performance) is below.

---

## ✨ Features

<table>
<tr>
<td width="50%">

### 🔍 Semantic Code Search
Describe what you need in plain English. Files are embedded with `all-MiniLM-L6-v2` (mean pooling, L2 normalized) and ranked by cosine similarity. The reply carries only the best-matching lines of the top files, with `@L<line>` markers where lines were skipped.

### 🎯 Exact Search
`find_in_codebase` does literal or regex search over the same safe file set, with a linear-time regex engine, so a bad pattern cannot hang the server.

### 💸 Lean Replies
Plain text, sent once, with the project root stated a single time and relative paths. No output schemas and no per-request telemetry.

</td>
<td width="50%">

### 🧠 Project Memory
`record_turn` and `record_session` append to a local chat ledger: JSON by default, or notes in an [Obsidian vault](#-storage-backends), or [Graphify](#-storage-backends) memory docs. `load_chat_context`, `load_cross_session_context` and `lookup_project_history` read it back. Search replies add a short "related past work" note when the history strongly matches the query.

### 🔐 Secrets Never Leave
Hard-coded filename patterns (`.env`, keys, credentials, ...) and content signatures (`API_KEY=`, `Bearer `, private key headers, ...) keep a file out of indexing, search and snippets. Messages that contain secrets are not recorded.

### ⚡ Incremental and Cached
Only files whose modification time or size changed are re-embedded. The index lives in `<project>/.cache/context-broker-rs/`, so a new process loads it instead of rebuilding.

</td>
</tr>
</table>

**Plus:** 🧵 batch-parallel embedding on a few worker threads · 💤 model and in-memory indexes released after 15 idle minutes · 🪦 exits when the client closes stdin · 🚫 honors `.gitignore` and `.ignore`, never follows symlinks · 📦 a single binary, no runtime library to install.

---

## 🚀 Quick Start

**Option 1: prebuilt static binary (Linux, no dependencies).** Each [release](https://github.com/InSelfControll/context-broker-rs/releases) carries a fully static (musl) binary for `x86_64` and `aarch64` that runs on any Linux distribution, with no libc or other library required:

```bash
curl -L https://github.com/InSelfControll/context-broker-rs/releases/latest/download/context-broker-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo install -m 755 context-broker /usr/local/bin/context-broker
```

Use `aarch64-unknown-linux-musl` in the file name on ARM. A `.sha256` file is published next to each archive.

**Option 2: build from source.** Needs a recent stable Rust (developed on 1.100; the 1.92 shipped by some distributions is too old for a dependency, so use the prebuilt binary there).

```bash
git clone https://github.com/InSelfControll/context-broker-rs.git
cd context-broker-rs
cargo build --release
```

The binary is `target/release/context-broker` (or wherever you installed the prebuilt one). It reads one JSON-RPC message per line on stdin and answers on stdout, so there is nothing to start by hand; your MCP client launches it. The command is just the binary path, with no arguments.

**Claude Code**

```bash
claude mcp add context-broker-rs -- /absolute/path/to/context-broker-rs/target/release/context-broker
```

**Codex CLI** (`~/.codex/config.toml`)

```toml
[mcp_servers.context-broker-rs]
command = "/absolute/path/to/context-broker-rs/target/release/context-broker"
```

<details>
<summary><b>Manual JSON configuration</b> (Claude Desktop, Cursor, and other clients that use <code>mcpServers</code>)</summary>

```json
{
  "mcpServers": {
    "context-broker-rs": {
      "command": "/absolute/path/to/context-broker-rs/target/release/context-broker"
    }
  }
}
```

To pin one project instead of auto-detecting it, add an environment variable:

```json
{
  "mcpServers": {
    "context-broker-rs": {
      "command": "/absolute/path/to/context-broker-rs/target/release/context-broker",
      "env": { "CONTEXT_BROKER_PROJECT_ROOT": "/path/to/project" }
    }
  }
}
```

</details>

<details>
<summary><b>The embedding model</b> (where the weights come from)</summary>

The model is `sentence-transformers/all-MiniLM-L6-v2` (`config.json`, `tokenizer.json`, `model.safetensors`). It is looked up in this order:

1. `CONTEXT_BROKER_MODEL_DIR`, a directory that already holds the three files.
2. An existing Hugging Face cache (`$HF_HOME/hub`, default `~/.cache/huggingface/hub`).
3. The broker's own cache, `~/.cache/context-broker/models/all-MiniLM-L6-v2` (override the base with `CONTEXT_BROKER_MODEL_CACHE_DIR`). If the files are missing they are downloaded once from `huggingface.co`, written to a `.part` file and renamed when complete.

If the Hugging Face cache already holds the model (for example from the Python broker), nothing is downloaded.

</details>

### Verify it works

Pipe two messages into the binary:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_context","arguments":{"query":"where is the main entry point","project_root":"/path/to/project"}}}' \
  | ./target/release/context-broker
```

The second line of output is a JSON-RPC result whose `content` holds the plain-text reply. Progress messages go to stderr, never to stdout.

---

## 📊 Performance

Measured on a 235-file repository, against the already-optimized Python server.

| | Python | Rust |
|---|---|---|
| Tool list each session pays for | 36 tools, 3,986 tokens | 12 tools, 1,191 tokens |
| Average search reply (6 queries) | ~900 tokens | ~660 tokens |
| Startup | 0.8 s | 2 ms |
| First search in a new process (index cached) | ~8 s | 78 ms |
| Warm search | 130-200 ms | 12-20 ms |
| Search after editing one file | 210 ms | 163 ms |
| Memory with model loaded | ~135 MB + ~890 MB worker | ~103 MB (about 200 MB after a full index) |
| Install | Python env + PyTorch | one 8 MB binary |

> **Honest limitation:** the first full index of a project is slow. It took about 12 s for 235 files on 4 worker threads, roughly 0.18 s of CPU per file. Afterwards only changed files are re-embedded and the index is cached on disk, so you pay this once per project (and again only for files you edit).

---

## 🧰 The Tools

Twelve tools are always listed. One more, `query_graph`, is listed when a Graphify graph is available (see [Storage backends](#-storage-backends)).

| Tool | Purpose |
|------|---------|
| [`search_context`](docs/tools/search_context.md) | Semantic search; best-matching lines of the top files. No query gives a project overview. |
| [`find_in_codebase`](docs/tools/find_in_codebase.md) | Exact literal or regex search; file, line and snippet. |
| [`route_task`](docs/tools/route_task.md) | Suggest which of these tools fit a task. |
| [`record_turn`](docs/tools/record_turn.md) | Append one user/assistant exchange to a session. |
| [`record_session`](docs/tools/record_session.md) | Append a whole conversation to a session. |
| [`load_chat_context`](docs/tools/load_chat_context.md) | Load one stored session, newest messages last. |
| [`load_cross_session_context`](docs/tools/load_cross_session_context.md) | Find stored messages across all sessions of the project. |
| [`lookup_project_history`](docs/tools/lookup_project_history.md) | Up to three past exchanges that match an issue. |
| [`save_search_results`](docs/tools/save_search_results.md) | Run a semantic search and save the reply under a name. |
| [`list_saved_results`](docs/tools/list_saved_results.md) | List saved search results. |
| [`load_saved_results`](docs/tools/load_saved_results.md) | Load a saved search result. |
| [`export_memory`](docs/tools/export_memory.md) | Copy the stored history and saved results into Obsidian or Graphify. |
| [`query_graph`](docs/tools/query_graph.md) | Graphify knowledge graph: a node's connections, or the shortest path between two nodes. |

See [docs/TOOLS.md](docs/TOOLS.md) for parameters and example replies.

---

## 🏗️ How It Works

```mermaid
flowchart LR
    Client["MCP client<br/>(agent)"] -->|"JSON-RPC line<br/>on stdin"| Main["main.rs<br/>dispatch"]
    Main --> Search["index.rs<br/>semantic search"]
    Main --> Literal["literal.rs<br/>regex / literal"]
    Main --> Memory["memory.rs<br/>ledger + results"]
    Main --> Graph["graph.rs<br/>Graphify graph"]
    Search --> Embed["embed.rs<br/>candle MiniLM"]
    Search --> Snip["snippet.rs<br/>line selection"]
    Search --> Files[("Project files")]
    Search --> Cache[("project/.cache/<br/>context-broker-rs")]
    Memory --> Store[("~/.context-broker,<br/>Obsidian vault or<br/>graphify-out/memory")]
    Graph --> GraphFile[("project/graphify-out/<br/>graph.json")]
    Main -->|"text blocks<br/>on stdout"| Client
```

1. **Collect.** Walk the project honoring ignore files; drop ignored directories, unsupported extensions, oversized files and secret-named files.
2. **Fingerprint.** Compare each file's modification time and size with the cached index. Unchanged rows are reused.
3. **Embed.** Changed files are read through the secret filter and embedded in batches across worker threads.
4. **Search.** The query is embedded, scored against every row, and the top files get a query-focused snippet within a token budget.

The details, including diagrams for the request flow and the storage layout, are in [ARCHITECTURE.md](ARCHITECTURE.md).

---

## 🗄️ Storage Backends

Chat memory and saved search results go to one store, chosen with `CONTEXT_BROKER_STORAGE`. The tools and their replies are the same for all three. `auto` picks for you: Obsidian when a vault is configured or found, else Graphify when the project has a `graphify-out/` folder or the `graphify` command is installed, else JSON.

| Store | Where it writes | Format |
|-------|-----------------|--------|
| `json` (default) | `~/.context-broker/` | One JSON ledger per session |
| `obsidian` | The vault's own `AI Chats/` structure when it has one, else `<vault>/Context Broker/<project>-<digest>/` | One Markdown note per session |
| `graphify` | `<project>/graphify-out/memory/` | One Markdown memory doc per exchange, in the layout of `graphify save-result` |

**Obsidian.** The vault is found automatically from Obsidian's own settings file (native, Flatpak, Snap, macOS and Windows locations): the vault that is open, else the most recently used one. So `CONTEXT_BROKER_STORAGE=obsidian` is enough; set `CONTEXT_BROKER_OBSIDIAN_VAULT` to choose a specific vault (that alone also selects this store). The vault found is logged once to stderr. Anything that syncs the vault, such as Obsidian Sync, carries the notes to your other devices. Notes open and render in Obsidian, can be linked and tagged like any other note, and edits you make there are read back. The broker only touches its own folder inside the vault.

```bash
claude mcp add -s user context-broker \
  -e CONTEXT_BROKER_STORAGE=obsidian -- context-broker-rs
```

**Following the vault.** The broker fits into a vault as it is already organised:

- If the vault files AI chats under `AI Chats/Chats/<platform>/`, the broker does the same. A session becomes `AI Chats/Chats/Context Broker/Context Broker <date> - <session> - <id>.md` with the same front matter as the other chats (`type: ai-chat`, `platform`, `title`, `created`, `updated`, `message_count`, `tags`), numbered `### 01. User — <time>` messages under `## Conversation`, and an entry in `AI Chats/Sources/AI Source - Context Broker.md` and `AI Chats/Projects/AI Project - <project>.md`. Chats imported from other platforms are never modified or read as broker history. Saved results go to `AI Chats/Results/<project>-<digest>/`.
- Otherwise it uses its own `Context Broker/` folder, placed inside the folder Obsidian is set to use for new notes ("Default location for new notes") when there is one. There each message is a callout.

`CONTEXT_BROKER_OBSIDIAN_LAYOUT=ai-chats` or `broker` overrides the choice.

**Graphify.** [Graphify](https://github.com/Graphify-Labs/graphify) turns a codebase into a knowledge graph in `graphify-out/`. With `CONTEXT_BROKER_STORAGE=graphify`:

- recorded exchanges become memory docs that Graphify folds into the graph on its next update;
- answers Graphify saved itself (`graphify save-result`) count as project history for `lookup_project_history` and the automatic "related past work" note;
- `query_graph` reads `graphify-out/graph.json` directly: a node's connections, or the shortest path between two nodes.

`query_graph` is also listed, whatever the store, when the project the server starts in already has a `graphify-out/graph.json`. The broker reads the graph and never builds it: run `/graphify .` in your assistant or `graphify extract .` first. `graphify-out/` is generated output and is left out of semantic search.

**Sending what is already stored.** History recorded in the JSON store is not left behind when you switch. Ask your assistant to export it, and `export_memory` copies every session and saved result of the project (or of all projects) into the vault or into Graphify memory docs, with the original timestamps. It can be repeated: only what is new is added.

The hosted Graphify Cloud graph is an MCP server of its own; connect it next to this one rather than through it: `claude mcp add --transport http graphify https://api.graphify.com/mcp`.

> This project is community-built and is not affiliated with or endorsed by Graphify Labs or Obsidian.

---

## 🌐 Remote Use (HTTP)

By default the server speaks MCP over stdio to a local agent. Set `CONTEXT_BROKER_HTTP_ADDR` and it serves the same tools over HTTP instead (the MCP Streamable HTTP transport, `POST /mcp`), for clients that connect over the network such as the ChatGPT and Claude apps.

```bash
CONTEXT_BROKER_HTTP_ADDR=127.0.0.1:8765 \
CONTEXT_BROKER_PROJECT_ROOT=/srv/obsidian/vault \
CONTEXT_BROKER_STORAGE=obsidian CONTEXT_BROKER_OBSIDIAN_VAULT=/srv/obsidian/vault \
context-broker-rs
```

- **Confined to one folder.** `CONTEXT_BROKER_PROJECT_ROOT` is required, and a `project_root` argument outside it is refused, so a remote caller cannot point the server at other folders.
- **`read_file`.** Remote clients cannot open files themselves, so this mode adds a tool that returns numbered lines of a file inside the project, through the same secret filter as search.
- **Keeping a remote copy of the vault in step.** `scripts/vault-sync.sh` is a small two-way sync through a git remote: run `vault-sync.sh init <vault> <remote-url>` once on each machine, then `vault-sync.sh <vault>` from a timer. Notes merge line by line and keep both sides' lines; every change stays in the git history.
- **Allowed addresses.** `CONTEXT_BROKER_HTTP_ALLOW_IPS` (addresses or CIDR ranges, comma separated) refuses every caller whose address is not listed. The caller is the connection's own address; only for a proxy on the same machine (a Cloudflare tunnel) is the `CF-Connecting-IP` header believed instead, so a direct caller cannot claim another address.
- **Several listen addresses.** `CONTEXT_BROKER_HTTP_ADDR` takes a comma-separated list, for example localhost for the tunnel plus a private-network address for machines on your own network.
- **Cloudflare Access sign-in.** With `CONTEXT_BROKER_ACCESS_TEAM_DOMAIN` and `CONTEXT_BROKER_ACCESS_AUD` set, a caller that Access has signed in is accepted whatever its address: the broker verifies the signed token Access attaches to the request (signature, application, issuer, expiry). A caller then needs a listed address or a valid sign-in.
- **File sync for the Obsidian plugin.** `CONTEXT_BROKER_SYNC=1` adds `/sync` endpoints next to `/mcp`: a manifest of the folder's files with content hashes, and read, write and delete of single files. A write or delete must name the content it replaces, so two devices cannot silently overwrite each other. Hidden files and folders (`.obsidian`, `.git`) are never listed or touched. The plugin in `obsidian-plugin/` uses these to keep a vault on a phone or another computer in step with the server.
- **No sign-in of its own.** Bind it to `127.0.0.1` and publish it only through a proxy that authenticates callers, for example a Cloudflare Tunnel with a Cloudflare Access application in front. Never bind it to a public address.

---

## ⚙️ Settings

All settings are environment variables. Names are shared with the Python broker where the feature exists in both.

| Variable | Default | Meaning |
|----------|---------|---------|
| `CONTEXT_BROKER_PROJECT_ROOT` | auto-detect | Project used when a call omits `project_root` |
| `CONTEXT_BROKER_RESULT_MAX_TOKENS_PER_FILE` | `240` | Snippet budget per file |
| `CONTEXT_BROKER_RESULT_SNIPPET_CONTEXT_LINES` | `2` | Context lines around a matching line |
| `CONTEXT_BROKER_EMBED_THREADS` | `min(4, cores/2)`, at least 1 | Worker threads for bulk embedding |
| `CONTEXT_BROKER_IDLE_RESOURCE_TIMEOUT_SECONDS` | `900` | Idle time before the model and indexes are released (`0` disables) |
| `CONTEXT_BROKER_INDEX_MAX_FILE_BYTES` | `2000000` | Skip larger files (`0` disables the limit) |
| `CONTEXT_BROKER_HTTP_ADDR` | none (stdio) | Serve MCP over HTTP on this address (or comma-separated addresses), for example `127.0.0.1:8765`; requires `CONTEXT_BROKER_PROJECT_ROOT` |
| `CONTEXT_BROKER_HTTP_ALLOW_IPS` | none | HTTP mode: only these caller addresses or CIDR ranges are served |
| `CONTEXT_BROKER_ACCESS_TEAM_DOMAIN` | none | HTTP mode: your Cloudflare Access team domain, `<team>.cloudflareaccess.com` |
| `CONTEXT_BROKER_ACCESS_AUD` | none | HTTP mode: the Access application's audience (AUD) tag |
| `CONTEXT_BROKER_SYNC` | off | HTTP mode: `1` enables the `/sync` file endpoints used by the Obsidian plugin |
| `CONTEXT_BROKER_STORAGE` | `json` | Store for chat memory and saved results: `json`, `obsidian`, `graphify` or `auto` |
| `CONTEXT_BROKER_STORAGE_DIR` | `~/.context-broker` | Where the `json` store lives |
| `CONTEXT_BROKER_OBSIDIAN_VAULT` | auto-detect | Obsidian vault folder; setting it selects the `obsidian` store |
| `CONTEXT_BROKER_OBSIDIAN_CONFIG` | standard locations | Obsidian's `obsidian.json`, when it is somewhere unusual |
| `CONTEXT_BROKER_OBSIDIAN_LAYOUT` | follow the vault | `ai-chats` (the vault's AI-chat structure) or `broker` (the broker's own folder) |
| `CONTEXT_BROKER_OBSIDIAN_FOLDER` | `Context Broker` | Folder for the broker's notes in the `broker` layout |
| `CONTEXT_BROKER_MODEL_DIR` | none | Directory with `config.json`, `tokenizer.json`, `model.safetensors` |
| `CONTEXT_BROKER_MODEL_CACHE_DIR` | `~/.cache/context-broker/models` | Where the model is downloaded once |
| `HF_HOME` | `~/.cache/huggingface` | Hugging Face cache searched before downloading |

<details>
<summary><b>Fixed limits</b> (constants, not settings)</summary>

| Limit | Value |
|-------|-------|
| Characters read per file for secret scan and snippets | 40,000 |
| Characters of each file that are embedded | 3,000 |
| Model input length | 256 tokens |
| Texts per inference batch | 8 |
| Indexed extensions | `md json toml yaml yml xml properties gradle go py ts js rs java nix html css scss sass less sh bash zsh fish ps1 sql graphql prisma` |

</details>

---

## 🔒 Privacy and Security

- **Local only.** The only network access is the one-time model download; nothing is sent anywhere afterwards.
- **Hard-coded secret rules.** `SECRET_FILE_PATTERNS`, `SECRET_KEY_SIGNATURES` and `SECRET_VALUE_SIGNATURES` live in `src/config.rs` and cannot be overridden by configuration or `.gitignore`.
- **Checked on every read.** The filename check and the content check run when files are collected, indexed, searched and snippeted, so a file that becomes secret-bearing later is dropped at the next change.
- **Memory is filtered too.** `record_turn` and `record_session` refuse a message containing secret signatures, and history lookups skip exchanges that contain them. Notes and memory docs can be edited outside the broker, so in the Obsidian and Graphify stores messages with secret signatures are also dropped when read.

---

## 🙅 Not Ported From the Python Broker

Honcho and Redis backends, the web dashboard, task delegation to worker models, model handoffs, the AGENTS.md / changelog / feature-docs generators, the environment doctor, the plan-execution tools, and the SSE / HTTP transports. If you need those, use the [Python Context Broker](https://github.com/InSelfControll/context-broker-mcp). Chat ledgers use the same layout (`<storage>/chats/<project digest>/<session>.json`), so history written by either server stays readable by the other.

---

## 📖 Documentation

| Document | What is in it |
|----------|---------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Module map, request flow, indexing pipeline, security layers, storage layout, threading |
| [docs/TOOLS.md](docs/TOOLS.md) | Index of the tools |
| [docs/REMOTE.md](docs/REMOTE.md) | Serving a vault or project to the Claude and ChatGPT apps: HTTP mode, who gets in, Cloudflare Tunnel and Access step by step, sync |
| [docs/tools/](docs/tools/) | One page per tool with parameters and an example reply |
| [docs/assets/make_gif.py](docs/assets/make_gif.py) | Generator for the demo GIF |

## 🧪 Development

```bash
cargo build --release
cargo test --release
```

One end-to-end test drives the real binary over MCP stdio through every tool, the secret and ignore policies, incremental indexing and persistence across a restart. It needs the embedding model (found in the Hugging Face cache, or downloaded once).

Static binaries are built by the `Static binaries` GitHub Actions workflow (`.github/workflows/release.yml`): run it manually for artifacts, or push a `v*` tag to publish a release. To build one yourself: `rustup target add x86_64-unknown-linux-musl`, install a musl C compiler (`musl-tools` on Debian/Ubuntu), then `cargo build --release --target x86_64-unknown-linux-musl`.

To rebuild the demo GIF: `uv run --with pillow python docs/assets/make_gif.py`.

## License

[MIT](LICENSE) © 2026 Ofir (InSelfControll)
