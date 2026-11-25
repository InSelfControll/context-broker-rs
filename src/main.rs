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
mod sync;

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
/// `project_root` is explained once, on the first tool. Defaults are applied by the server and
/// not repeated in the schemas.
type Tool = (&'static str, &'static str, Value, &'static [&'static str]);

/// The tools a client is told about. `route_task`, `record_turn` and `list_saved_results` are
/// still answered by `call` for clients that know them, but are no longer listed: the listed
/// tools cover what they do.
fn tools() -> Vec<Tool> {
    let text = || json!({"type": "string"});
    let int = || json!({"type": "integer"});
    let flag = || json!({"type": "boolean"});
    let mut list: Vec<Tool> = vec![
        ("search_context", "Semantic code search: best-matching lines of the top files, with @L line markers. Omit query for a project overview. project_root is optional on every tool (auto-detected).",
            json!({"query": text(), "top_k": int(), "project_root": text()}), &[]),
        ("find_in_codebase", "Exact literal or regex search for a known symbol or string.",
            json!({"pattern": text(), "use_regex": flag(), "case_sensitive": flag(), "file_glob": text(), "project_root": text()}), &["pattern"]),
        ("record_session", "Save exchanges to a session. Only when the user asks or a decision is worth keeping, not after every reply. Summarize long replies.",
            json!({"session_id": text(), "project_root": text(),
                   "turns": {"type": "array", "items": {"type": "object", "properties": {"user": text(), "assistant": text()}}}}), &["session_id", "turns"]),
        ("load_chat_context", "Load a stored session.",
            json!({"session_id": text(), "search_query": text(), "limit": int(), "project_root": text()}), &["session_id"]),
        ("load_cross_session_context", "Find stored messages containing search_query in all sessions.",
            json!({"search_query": text(), "top_k": int(), "project_root": text()}), &["search_query"]),
        ("lookup_project_history", "Was this issue handled before? Up to three past exchanges.",
            json!({"query": text(), "project_root": text()}), &["query"]),
        ("save_search_results", "Run a semantic search and save the reply under a name.",
            json!({"query": text(), "filename": text(), "top_k": int(), "project_root": text()}), &["query", "filename"]),
        ("load_saved_results", "Load a saved search result. Omit filename to list them.",
            json!({"filename": text(), "project_root": text()}), &[]),
        ("export_memory", "Copy stored history and saved results into Obsidian or Graphify. Only when the user asks; repeating adds nothing.",
            json!({"target": {"type": "string", "enum": ["obsidian", "graphify"]}, "all_projects": flag(), "project_root": text()}), &[]),
    ];
    if REMOTE.get().is_some() {
        // Remote clients have no file access of their own; local agents read files themselves.
        list.push(("read_file", "Numbered lines of a project file, by the relative path search results give.",
            json!({"path": text(), "start_line": int(), "max_lines": int(), "project_root": text()}), &["path"]));
    }
    if graph_tool_listed() {
        list.push(("query_graph", "Graphify graph: a node's connections (callers, dependencies), or with `to` the shortest path between two nodes.",
            json!({"node": text(), "to": text(), "limit": int(), "project_root": text()}), &["node"]));
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
    // Judged from the served folder, whatever sub-folder the caller chose as project_root.
    let served = project::locked_root().unwrap_or(root);
    if project::relative(served, &file).split('/').any(|part| config::PRIVATE_DIRS.contains(&part)) {
        return Err(format!("cannot read {rel}: private folder"));
    }
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
            let mut schema = json!({"type": "object", "properties": properties});
            if !required.is_empty() {
                schema["required"] = json!(required);
            }
            json!({"name": name, "description": description, "inputSchema": schema})
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
        "load_saved_results" => match arg(args, "filename") {
            "" => memory::list_results(&root)?,
            filename => memory::load_results(&root, filename)?,
        },
        "read_file" => read_file(&root, arg(args, "path"), arg_usize(args, "start_line", 1), arg_usize(args, "max_lines", 200))?,
        "export_memory" => {
            let all_projects = args.get("all_projects").and_then(Value::as_bool).unwrap_or(false);
            // Remote callers are confined to the served folder; other projects are out of reach.
            if all_projects && project::locked_root().is_some() {
                return Err("all_projects is not available over HTTP: the server is confined to one folder".into());
            }
            memory::export(&root, arg(args, "target"), all_projects)?
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
    // File sync for remote copies of the folder is opt-in: it lets callers write and delete files.
    let files = Arc::new(std::env::var("CONTEXT_BROKER_SYNC").is_ok_and(|v| v.trim() == "1").then(|| sync::Sync::new(root.clone())));
    if files.is_some() {
        eprintln!("[broker] file sync enabled at /sync");
    }
    let hosts = Arc::new(config::http_allowed_hosts());
    let open = allowed.is_empty() && verifier.is_none();
    let insecure = std::env::var("CONTEXT_BROKER_HTTP_ALLOW_OPEN").is_ok_and(|v| v.trim() == "1");
    let mut listeners = Vec::new();
    for address in addresses.split(',').map(str::trim).filter(|address| !address.is_empty()) {
        // With no allow-list and no sign-in, anyone who can reach the port can read the folder
        // (and, with sync, write it): only serve that on loopback unless explicitly asked to.
        let public = std::net::ToSocketAddrs::to_socket_addrs(address).map_or(true, |mut resolved| resolved.any(|socket| !socket.ip().is_loopback()));
        if open && public && !insecure {
            return Err(format!("refusing to serve {address} to any caller: set CONTEXT_BROKER_HTTP_ALLOW_IPS or Cloudflare Access, bind 127.0.0.1, or set CONTEXT_BROKER_HTTP_ALLOW_OPEN=1"));
        }
        let server = tiny_http::Server::http(address).map_err(|e| format!("cannot listen on {address}: {e}"))?;
        eprintln!("[broker] MCP over HTTP at http://{address}/mcp, confined to {}, {}", root.display(), if allowed.is_empty() { "any caller address".to_string() } else { format!("{} allowed network(s)", allowed.len()) });
        let (app, allowed, hosts, verifier, files) = (app.clone(), allowed.clone(), hosts.clone(), verifier.clone(), files.clone());
        listeners.push(std::thread::spawn(move || serve(&app, &server, &allowed, &hosts, verifier.as_ref().as_ref(), files.as_ref().as_ref())));
    }
    for listener in listeners {
        let _ = listener.join();
    }
    Ok(())
}

/// Answer one `/sync/...` request: `manifest`, or `file?path=...` with GET, PUT or DELETE.
fn sync_reply(files: &sync::Sync, request: &mut tiny_http::Request, route: &str, query: &str) -> sync::Reply {
    let header = |name: &'static str| request.headers().iter().find(|header| header.field.equiv(name)).map(|header| header.value.as_str().trim().to_string()).unwrap_or_default();
    let path = query.split('&').find_map(|pair| pair.strip_prefix("path=")).map(sync::percent_decode).unwrap_or_default();
    let base = header("X-Base-Hash");
    match (request.method().clone(), route) {
        (tiny_http::Method::Get, "manifest") => files.manifest(),
        (tiny_http::Method::Get, "file") => files.read(&path),
        (tiny_http::Method::Delete, "file") => files.delete(&path, &base),
        (tiny_http::Method::Put, "file") => {
            let mut body = Vec::new();
            match request.as_reader().take(sync::MAX_FILE_BYTES + 1).read_to_end(&mut body) {
                Ok(_) if body.len() as u64 <= sync::MAX_FILE_BYTES => files.write(&path, &base, &body),
                Ok(_) => sync::Reply::Json(413, json!({"error": "file too large"})),
                Err(_) => sync::Reply::Json(400, json!({"error": "unreadable request body"})),
            }
        }
        _ => sync::Reply::Json(404, json!({"error": "not found"})),
    }
}

/// The host part of a `Host` header or an origin's authority, lowercased, without the port.
fn host_part(authority: &str) -> String {
    let authority = authority.trim().to_ascii_lowercase();
    match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or("").to_string(),
        None if authority.matches(':').count() > 1 => authority,
        None => authority.split(':').next().unwrap_or("").to_string(),
    }
}

/// A host the server answers to: an address literal, `localhost`, or one listed in
/// `CONTEXT_BROKER_HTTP_ALLOW_HOSTS`. Anything else is a DNS name some other site pointed at
/// this machine (DNS rebinding), so a browser page there must not reach the server.
fn known_host(host: &str, hosts: &[String]) -> bool {
    host.parse::<std::net::IpAddr>().is_ok() || host == "localhost" || host.ends_with(".localhost") || hosts.iter().any(|allowed| allowed == host)
}

/// Browsers name the page that sent a request in `Origin`. Server-side clients send none; a page
/// from another site must not drive the tools (cross-site request forgery).
fn known_origin(origin: &str, hosts: &[String]) -> bool {
    let Some((scheme, rest)) = origin.trim().split_once("://") else { return false };
    let host = host_part(rest.split('/').next().unwrap_or(""));
    match scheme.to_ascii_lowercase().as_str() {
        // The Obsidian apps (desktop and mobile).
        "app" => host == "obsidian.md",
        "capacitor" => host == "localhost",
        "http" | "https" => host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) || hosts.iter().any(|allowed| *allowed == host),
        _ => false,
    }
}

fn serve(app: &Mutex<App>, server: &tiny_http::Server, allowed: &[config::Network], hosts: &[String], verifier: Option<&access::Verifier>, files: Option<&sync::Sync>) {
    let json_type = tiny_http::Header::from_bytes("Content-Type", "application/json").expect("valid header");
    for mut request in server.incoming_requests() {
        let plain = |status: u16, text: &str| tiny_http::Response::from_string(text).with_status_code(status);
        let url = request.url().to_string();
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
        let header = |name: &'static str| request.headers().iter().find(|header| header.field.equiv(name)).map(|header| header.value.as_str().to_string());
        let signed_in = peer.is_some_and(|ip| ip.is_loopback())
            && verifier.is_some_and(|verifier| header("Cf-Access-Jwt-Assertion").is_some_and(|token| verifier.accepts(&token)));
        let restricted = !allowed.is_empty() || verifier.is_some();
        // A request vouched for by Access came through the tunnel, under its public hostname.
        let host_ok = signed_in || header("Host").is_none_or(|host| known_host(&host_part(&host), hosts));
        let origin_ok = header("Origin").is_none_or(|origin| known_origin(&origin, hosts));
        let response = if !host_ok || !origin_ok {
            plain(403, "not allowed: unknown Host or Origin (see CONTEXT_BROKER_HTTP_ALLOW_HOSTS)")
        } else if restricted && !(!allowed.is_empty() && listed) && !signed_in {
            plain(403, "not allowed: unknown address and no valid sign-in")
        } else if let (Some(files), Some(route)) = (files, url.split('?').next().and_then(|path| path.strip_prefix("/sync/"))) {
            match sync_reply(files, &mut request, route, url.split_once('?').map_or("", |(_, query)| query)) {
                sync::Reply::Json(status, body) => plain(status, &body.to_string()).with_header(json_type.clone()),
                sync::Reply::File(bytes, hash) => tiny_http::Response::from_data(bytes)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", "application/octet-stream").expect("valid header"))
                    .with_header(tiny_http::Header::from_bytes("X-Content-Hash", hash).expect("valid header")),
            }
        } else if url.split('?').next() != Some("/mcp") {
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
