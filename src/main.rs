//! Context Broker: a token-lean MCP server for semantic code search and project memory.
//!
//! Speaks MCP (JSON-RPC 2.0, one message per line) over stdio. Replies are plain text, sent
//! once, with no telemetry. The process exits when stdin closes.

mod config;
mod embed;
mod index;
mod literal;
mod memory;
mod project;
mod security;
mod snippet;

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_QUERY: &str = "main entry point configuration setup architecture";

struct App {
    engine: index::Engine,
    last_used: Instant,
}

/// `(name, description, properties, required)` — kept short: every session pays for this list.
fn tools() -> Vec<(&'static str, &'static str, Value, &'static [&'static str])> {
    let root = || json!({"type": "string", "description": "Project root; auto-detected if omitted"});
    let text = || json!({"type": "string"});
    vec![
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
    ]
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
        "lookup_project_history" => memory::history_lookup(&root, arg(args, "query")),
        "list_saved_results" => memory::list_results(&root),
        "load_saved_results" => memory::load_results(&root, arg(args, "filename"))?,
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
                && app.last_used.elapsed() >= Duration::from_secs(timeout) && app.engine.release() {
                    index::trim_memory();
                    eprintln!("[broker] released idle model and index memory");
                }
        }
    });
}

fn main() {
    // Embedding parallelism is across batches (see embed.rs); keep the inner pool single-threaded
    // so worker threads do not oversubscribe the machine.
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("RAYON_NUM_THREADS", "1") };
    }
    let app = Arc::new(Mutex::new(App { engine: index::Engine::default(), last_used: Instant::now() }));
    spawn_idle_release(app.clone());
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Err(error) => json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": error.to_string()}}),
            Ok(message) => {
                // Notifications (no id) never get a response.
                let Some(id) = message.get("id").cloned() else { continue };
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                match handle(&app, method, message.get("params").unwrap_or(&Value::Null)) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, text)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}}),
                }
            }
        };
        if writeln!(stdout, "{response}").and_then(|_| stdout.flush()).is_err() {
            break;
        }
    }
}
