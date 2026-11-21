//! Context Broker: a token-lean MCP server for semantic code search and project memory.
//!
//! Speaks MCP (JSON-RPC 2.0, one message per line) over stdio. Replies are plain text, sent
//! once, with no telemetry. The process exits when stdin closes.

mod access;
mod config;
mod embed;
mod graph;
mod index;
mod literal;
mod memory;
mod project;
mod security;
mod snippet;

use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_QUERY: &str = "main entry point configuration setup architecture";

struct App {
    engine: index::Engine,
    graphs: graph::Graphs,
    last_used: Instant,
}

/// `(name, description, properties, required)` — kept short: every session pays for this list.
type Tool = (&'static str, &'static str, Value, &'static [&'static str]);

fn tools() -> Vec<Tool> {
    let root = || json!({"type": "string", "description": "Project root; auto-detected if omitted"});
    let text = || json!({"type": "string"});
    let mut list: Vec<Tool> = vec![
        ("search_context", "Semantic code search. Returns the best-matching lines of the top files as plain text with relative paths and @L line markers. Omit query for a project overview.",
            json!({"query": text(), "project_root": root(), "top_k": {"type": "integer", "default": 5}}), &[]),
        ("find_in_codebase", "Exact literal or regex search; returns file, line and snippet. Use for a known symbol or string.",
            json!({"pattern": text(), "project_root": root(), "case_sensitive": {"type": "boolean", "default": false},
                   "use_regex": {"type": "boolean", "default": false}, "file_glob": text()}), &["pattern"]),
        ("route_task", "Suggest which of this server's tools fit a task.",
            json!({"task": text(), "top_k": {"type": "integer", "default": 3}}), &["task"]),
        ("record_turn", "Append one (user, assistant) exchange to a session. Call only when the user asks to save or for a decision worth keeping; do NOT call after every response.",
            json!({"session_id": text(), "user_message": text(), "assistant_message": text(), "project_root": root()}), &["session_id"]),
        ("record_session", "Append a whole conversation to a session in one call. Summarize long replies instead of repeating them.",
            json!({"session_id": text(), "project_root": root(),
                   "turns": {"type": "array", "items": {"type": "object", "properties": {"user": text(), "assistant": text()}}}}), &["session_id", "turns"]),
        ("load_chat_context", "Load a stored session (newest messages last).",
            json!({"session_id": text(), "project_root": root(), "search_query": text(), "limit": {"type": "integer", "default": 20}}), &["session_id"]),
        ("load_cross_session_context", "Find stored messages containing search_query across all sessions of the project.",
            json!({"search_query": text(), "project_root": root(), "top_k": {"type": "integer", "default": 10}}), &["search_query"]),
        ("lookup_project_history", "Check whether this issue was handled before; returns up to three past exchanges.",
            json!({"query": text(), "project_root": root()}), &["query"]),
        ("save_search_results", "Run a semantic search and save the result under a name.",
            json!({"query": text(), "filename": text(), "project_root": root(), "top_k": {"type": "integer", "default": 5}}), &["query", "filename"]),
        ("list_saved_results", "List saved search results for the project.", json!({"project_root": root()}), &[]),
        ("load_saved_results", "Load a saved search result.", json!({"filename": text(), "project_root": root()}), &["filename"]),
        ("export_memory", "Copy the stored chat history and saved results into Obsidian or Graphify (auto-detected when target is omitted). Call only when the user asks; repeating it adds nothing.",
            json!({"target": {"type": "string", "enum": ["obsidian", "graphify"]}, "all_projects": {"type": "boolean", "default": false}, "project_root": root()}), &[]),
    ];
    if REMOTE.get().is_some() {
        // Remote clients have no file access of their own; local agents read files themselves.
        list.push(("read_file", "Read a file of the project by relative path (as given in search results). Returns numbered lines.",
            json!({"path": text(), "start_line": {"type": "integer", "default": 1}, "max_lines": {"type": "integer", "default": 200}, "project_root": root()}), &["path"]));
    }
    if graph_tool_listed() {
        list.push(("query_graph", "Graphify knowledge graph: a node's connections, or with `to` the shortest path between two nodes. Use for callers, dependencies and how things connect.",
            json!({"node": text(), "to": text(), "project_root": root(), "limit": {"type": "integer", "default": 20}}), &["node"]));
    }
    list
}

/// Set when the server speaks HTTP to remote clients.
static REMOTE: OnceLock<()> = OnceLock::new();
const MAX_REQUEST_BYTES: u64 = 1_000_000;
const MAX_READ_CHARS: usize = 400_000;
const MAX_READ_REPLY_CHARS: usize = 12_000;

/// Numbered lines of one project file, through the same secret filter as search.
fn read_file(root: &std::path::Path, path: &str, start_line: usize, max_lines: usize) -> Result<String, String> {
    let file = project::inside(root, path)?;
    let rel = project::relative(root, &file);
    let content = project::read_safe(root, &file, MAX_READ_CHARS).filter(|_| file.is_file()).ok_or_else(|| format!("cannot read {rel}: missing, binary or blocked as secret-bearing"))?;
    let lines: Vec<&str> = content.lines().collect();
    let first = start_line.clamp(1, lines.len().max(1));
    let mut out = String::new();
    let mut last = first - 1;
    for (number, line) in lines.iter().enumerate().skip(first - 1).take(max_lines.clamp(1, 2_000)) {
        if out.len() + line.len() > MAX_READ_REPLY_CHARS {
            break;
        }
        out.push_str(&format!("\n{}: {line}", number + 1));
        last = number + 1;
    }
    Ok(format!("{rel} · lines {first}-{last} of {}{out}", lines.len()))
}

/// `query_graph` is listed only where it can answer: with Graphify storage, or when the project
/// the server started in already has a graph. It stays callable either way.
fn graph_tool_listed() -> bool {
    static LISTED: OnceLock<bool> = OnceLock::new();
    *LISTED.get_or_init(|| {
        project::resolve_root("")
            .is_ok_and(|root| matches!(config::store(&root), Ok(config::Store::Graphify)) || graph::graph_file(&root).is_file())
    })
}

fn tool_list() -> Value {
    let list: Vec<Value> = tools()
        .into_iter()
        .map(|(name, description, properties, required)| {
            json!({"name": name, "description": description,
                   "inputSchema": {"type": "object", "properties": properties, "required": required}})
        })
        .collect();
    json!({"tools": list})
}

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key).and_then(Value::as_u64).map_or(default, |n| n as usize)
}

/// Rank this server's tools by words shared with the task.
fn route(task: &str, top_k: usize) -> String {
    let lowered = task.to_lowercase();
    let words = snippet::query_terms(task);
    let literal = ["exact", "literal", "grep", "regex", "where is", "which file", "which line", "def ", "class ", "import "]
        .iter()
        .any(|signal| lowered.contains(signal));
    let mut ranked: Vec<(usize, &str, &str)> = tools()
        .into_iter()
        .filter(|(name, ..)| *name != "route_task")
        .map(|(name, description, ..)| {
            let haystack = format!("{} {}", name.replace('_', " "), description.to_lowercase());
            let mut score = words.iter().filter(|word| haystack.contains(word.as_str())).count();
            if literal && name == "find_in_codebase" {
                score += 10;
            }
            (score, name, description)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    if ranked.first().is_none_or(|(score, ..)| *score == 0) {
        return "search_context — default for exploring code by meaning".into();
    }
    ranked
        .into_iter()
        .filter(|(score, ..)| *score > 0)
        .take(top_k.clamp(1, 5))
        .map(|(_, name, description)| format!("{name} — {description}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run one tool; `Ok` carries the content blocks of the reply.
fn call(app: &mut App, name: &str, args: &Value) -> Result<Vec<String>, String> {
    if name == "route_task" {
        return Ok(vec![route(arg(args, "task"), arg_usize(args, "top_k", 3))]);
    }
    let root = project::resolve_root(arg(args, "project_root"))?;
    let reply = match name {
        "search_context" | "save_search_results" => {
            let query = match arg(args, "query") {
                "" if name == "search_context" => DEFAULT_QUERY,
                query => query,
            };
            let text = index::render(&app.engine.search(&root, query, arg_usize(args, "top_k", 5))?);
            if name == "save_search_results" {
                return memory::save_results(&root, arg(args, "filename"), query, &text).map(|saved| vec![saved]);
            }
            text
        }
        "find_in_codebase" => literal::search(
            &root,
            arg(args, "pattern"),
            args.get("case_sensitive").and_then(Value::as_bool).unwrap_or(false),
            args.get("use_regex").and_then(Value::as_bool).unwrap_or(false),
            arg(args, "file_glob"),
        )?,
        "record_turn" => memory::record(
            &root,
            arg(args, "session_id"),
            &[(arg(args, "user_message").to_string(), arg(args, "assistant_message").to_string())],
        )?,
        "record_session" => {
            let turns: Vec<(String, String)> = args
                .get("turns")
                .and_then(Value::as_array)
                .ok_or("turns must be a list of {user, assistant} objects")?
                .iter()
                .map(|turn| (arg(turn, "user").to_string(), arg(turn, "assistant").to_string()))
                .collect();
            memory::record(&root, arg(args, "session_id"), &turns)?
        }
        "load_chat_context" => {
            memory::load_session(&root, arg(args, "session_id"), arg(args, "search_query"), arg_usize(args, "limit", 20))?
        }
        "load_cross_session_context" => memory::cross_session(&root, arg(args, "search_query"), arg_usize(args, "top_k", 10))?,
        "lookup_project_history" => memory::history_lookup(&root, arg(args, "query"), &mut |texts| app.engine.embed(texts)),
        "list_saved_results" => memory::list_results(&root)?,
        "load_saved_results" => memory::load_results(&root, arg(args, "filename"))?,
        "read_file" => read_file(&root, arg(args, "path"), arg_usize(args, "start_line", 1), arg_usize(args, "max_lines", 200))?,
        "export_memory" => {
            memory::export(&root, arg(args, "target"), args.get("all_projects").and_then(Value::as_bool).unwrap_or(false))?
        }
        "query_graph" => {
            let graph = app.graphs.get(&root)?;
            match arg(args, "to") {
                "" => graph.explain(arg(args, "node"), arg_usize(args, "limit", 20)),
                to => graph.path(arg(args, "node"), to),
            }
        }
        other => return Err(format!("unknown tool: {other}")),
    };
    let mut blocks = vec![reply];
    // Search replies carry a short pointer to related past work, when there is any.
    let question = match name {
        "search_context" => arg(args, "query"),
        "find_in_codebase" => arg(args, "pattern"),
        _ => "",
    };
    if !question.is_empty() {
        blocks.extend(memory::history_note(&root, question));
    }
    Ok(blocks)
}

fn handle(app: &Mutex<App>, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if SUPPORTED_PROTOCOLS.contains(&requested) { requested } else { SUPPORTED_PROTOCOLS[1] };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "context-broker", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tool_list()),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let empty = json!({});
            let args = params.get("arguments").unwrap_or(&empty);
            let mut app = app.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let outcome = call(&mut app, name, args);
            app.last_used = Instant::now();
            let (blocks, is_error) = match outcome {
                Ok(blocks) => (blocks, false),
                Err(message) => (vec![format!("Error: {message}")], true),
            };
            let content: Vec<Value> = blocks.into_iter().map(|text| json!({"type": "text", "text": text})).collect();
            Ok(json!({"content": content, "isError": is_error}))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

/// Free the model and in-memory indexes after a quiet period.
fn spawn_idle_release(app: Arc<Mutex<App>>) {
    let timeout = config::idle_timeout_secs();
    if timeout == 0 {
        return;
    }
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(timeout.clamp(1, 30)));
            if let Ok(mut app) = app.try_lock()
                && app.last_used.elapsed() >= Duration::from_secs(timeout) && (app.engine.release() | app.graphs.release()) {
                    index::trim_memory();
                    eprintln!("[broker] released idle model and index memory");
                }
        }
    });
}

/// Answer one JSON-RPC message; `None` for notifications, which get no reply.
fn respond(app: &Mutex<App>, line: &str) -> Option<Value> {
    Some(match serde_json::from_str::<Value>(line) {
        Err(error) => json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": error.to_string()}}),
        Ok(message) => {
            let id = message.get("id").cloned()?;
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            match handle(app, method, message.get("params").unwrap_or(&Value::Null)) {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, text)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}}),
            }
        }
    })
}

/// MCP over HTTP (the Streamable HTTP transport, JSON replies): `POST /mcp` with one JSON-RPC
/// message, on one or more comma-separated listen addresses. There is no sign-in here: listen on
/// localhost or a private network, limit callers with the allow-list, and put an authenticating
/// proxy in front for anything public.
fn serve_http(app: Arc<Mutex<App>>, addresses: &str) -> Result<(), String> {
    let root = project::lock_root()?;
    let _ = REMOTE.set(());
    let allowed = Arc::new(config::http_allowed_networks()?);
    let verifier = Arc::new(access::Verifier::from_env()?);
    if verifier.is_some() {
        eprintln!("[broker] callers signed in through Cloudflare Access are accepted");
    }
    let mut listeners = Vec::new();
    for address in addresses.split(',').map(str::trim).filter(|address| !address.is_empty()) {
        let server = tiny_http::Server::http(address).map_err(|e| format!("cannot listen on {address}: {e}"))?;
        eprintln!("[broker] MCP over HTTP at http://{address}/mcp, confined to {}, {}", root.display(), if allowed.is_empty() { "any caller address".to_string() } else { format!("{} allowed network(s)", allowed.len()) });
        let (app, allowed, verifier) = (app.clone(), allowed.clone(), verifier.clone());
        listeners.push(std::thread::spawn(move || serve(&app, &server, &allowed, verifier.as_ref().as_ref())));
    }
    for listener in listeners {
        let _ = listener.join();
    }
    Ok(())
}

fn serve(app: &Mutex<App>, server: &tiny_http::Server, allowed: &[config::Network], verifier: Option<&access::Verifier>) {
    let json_type = tiny_http::Header::from_bytes("Content-Type", "application/json").expect("valid header");
    for mut request in server.incoming_requests() {
        let plain = |status: u16, text: &str| tiny_http::Response::from_string(text).with_status_code(status);
        // Who is calling: the connection's own address, except for a proxy on this machine (a
        // Cloudflare tunnel), which reports the real caller in CF-Connecting-IP. That header is
        // believed only from localhost, so a direct caller cannot claim another address.
        let peer = request.remote_addr().map(|address| address.ip());
        let caller = match (peer, request.headers().iter().find(|header| header.field.equiv("CF-Connecting-IP"))) {
            (Some(ip), Some(forwarded)) if ip.is_loopback() => forwarded.value.as_str().trim().parse().ok(),
            (peer, _) => peer,
        };
        // A caller gets in by address, or by a valid Cloudflare Access token. Like the address
        // header, the token header is only read from the local proxy.
        let listed = caller.is_some_and(|ip| allowed.iter().any(|network| config::network_contains(network, ip)));
        let signed_in = || {
            let token = request.headers().iter().find(|header| header.field.equiv("Cf-Access-Jwt-Assertion"));
            peer.is_some_and(|ip| ip.is_loopback()) && verifier.is_some_and(|verifier| token.is_some_and(|token| verifier.accepts(token.value.as_str())))
        };
        let restricted = !allowed.is_empty() || verifier.is_some();
        let response = if restricted && !(!allowed.is_empty() && listed) && !signed_in() {
            plain(403, "not allowed: unknown address and no valid sign-in")
        } else if request.url().split('?').next() != Some("/mcp") {
            plain(404, "not found")
        } else if *request.method() != tiny_http::Method::Post {
            plain(405, "use POST").with_header(tiny_http::Header::from_bytes("Allow", "POST").expect("valid header"))
        } else {
            let mut body = String::new();
            match request.as_reader().take(MAX_REQUEST_BYTES).read_to_string(&mut body) {
                Err(_) => plain(400, "unreadable request body"),
                Ok(_) => match respond(app, &body) {
                    Some(reply) => plain(200, &reply.to_string()).with_header(json_type.clone()),
                    None => plain(202, ""),
                },
            }
        };
        let _ = request.respond(response);
    }
}

fn main() {
    // Embedding parallelism is across batches (see embed.rs); keep the inner pool single-threaded
    // so worker threads do not oversubscribe the machine.
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("RAYON_NUM_THREADS", "1") };
    }
    let app = Arc::new(Mutex::new(App { engine: index::Engine::default(), graphs: graph::Graphs::default(), last_used: Instant::now() }));
    spawn_idle_release(app.clone());
    if let Some(address) = std::env::var("CONTEXT_BROKER_HTTP_ADDR").ok().filter(|v| !v.trim().is_empty()) {
        if let Err(error) = serve_http(app, address.trim()) {
            eprintln!("[broker] {error}");
            std::process::exit(1);
        }
        return;
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = respond(&app, &line) else { continue };
        if writeln!(stdout, "{response}").and_then(|_| stdout.flush()).is_err() {
            break;
        }
    }
}
