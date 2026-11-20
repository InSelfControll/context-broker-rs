//! One end-to-end test: drives the real binary over MCP stdio through every tool and policy.
//!
//! Needs the all-MiniLM-L6-v2 model (found in the Hugging Face cache, or downloaded once).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(storage: &Path, log: &Path) -> Self {
        let log_file = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_context-broker"))
            .env("CONTEXT_BROKER_STORAGE_DIR", storage)
            .env_remove("CONTEXT_BROKER_PROJECT_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log_file)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next_id: 0 }
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(!line.contains("\n\n") && line.ends_with('\n'), "one JSON message per line");
        serde_json::from_str(&line).unwrap()
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.send_raw(&json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}).to_string());
        let reply = self.read();
        assert_eq!(reply["id"], self.next_id, "reply matches request");
        reply
    }

    /// Returns `(content blocks, isError)`.
    fn call(&mut self, name: &str, args: Value) -> (Vec<String>, bool) {
        let reply = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        let result = &reply["result"];
        assert!(result.get("structuredContent").is_none(), "replies are sent once, as text");
        let blocks = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| {
                assert_eq!(block["type"], "text");
                block["text"].as_str().unwrap().to_string()
            })
            .collect();
        (blocks, result["isError"].as_bool().unwrap())
    }

    fn ok(&mut self, name: &str, args: Value) -> String {
        let (blocks, is_error) = self.call(name, args);
        assert!(!is_error, "{name} failed: {blocks:?}");
        blocks[0].clone()
    }

    /// Closing stdin must end the process cleanly.
    fn shutdown(mut self) {
        drop(self.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn fixture() -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("context-broker-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (root, storage) = (base.join("project"), base.join("storage"));
    let filler: String = (0..60).map(|i| format!("import helper_{i}\n")).collect();
    write(&root, "pyproject.toml", "[project]\nname = \"demo\"\n");
    write(&root, "src/auth.py", &format!("{filler}\ndef login_user(name, password_hash):\n    \"\"\"Authenticate a user and start a session.\"\"\"\n    return sessions.start(name)\n\n{filler}"));
    write(&root, "src/billing.py", "def charge_card(amount):\n    \"\"\"Charge a customer credit card for an invoice.\"\"\"\n    return gateway.charge(amount)\n");
    write(&root, "docs/weather.md", "# Weather\nNotes about rain, clouds and sunshine forecasts.\n");
    write(&root, ".env", "DATABASE_URL=postgres://secret-in-dotenv\n");
    write(&root, "src/settings.py", "API_KEY=abc123secretvalue\ndef configure():\n    return login_user\n");
    write(&root, "node_modules/lib/index.js", "function login_user() { return 'ignored-dependency'; }\n");
    write(&root, "src/data.bin", "login_user unsupported extension\n");
    (base.clone(), root.canonicalize().unwrap(), storage)
}

#[test]
fn full_mcp_session() {
    let (base, root, storage) = fixture();
    let log = base.join("server.log");
    let root_arg = root.to_str().unwrap();
    let mut server = Server::start(&storage, &log);

    // --- protocol -----------------------------------------------------------------------------
    let init = server.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}}));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "context-broker");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    let fallback = server.rpc("initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(fallback["result"]["protocolVersion"], "2025-06-18");
    // A notification gets no reply: the next message read answers the ping.
    server.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert_eq!(server.rpc("ping", json!({}))["result"], json!({}));
    assert_eq!(server.rpc("resources/list", json!({}))["error"]["code"], -32601);
    server.send_raw("{not json");
    assert_eq!(server.read()["error"]["code"], -32700);

    let listing = server.rpc("tools/list", json!({}));
    let tools = listing["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["search_context", "find_in_codebase", "route_task", "record_turn", "record_session", "load_chat_context",
         "load_cross_session_context", "lookup_project_history", "save_search_results", "list_saved_results", "load_saved_results"]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(tool.get("outputSchema").is_none(), "no output schema: replies are plain text");
    }
    assert!(listing.to_string().len() < 5_000, "tool listing stays small");

    // --- semantic search: relevance, lean format, secret and ignore policy -----------------------
    let found = server.ok("search_context", json!({"query": "authenticate user login session", "project_root": root_arg, "top_k": 2}));
    let mut lines = found.lines();
    assert_eq!(lines.next().unwrap(), format!("2 of 4 files · root: {root_arg}"));
    assert_eq!(lines.next().unwrap(), "### src/auth.py", "best match first, relative path");
    assert!(found.contains("@L") && found.contains("def login_user(name, password_hash):"));
    assert!(!found.contains("helper_30"), "snippet is focused on the match, not the whole file");
    assert_eq!(found.matches(root_arg).count(), 1, "root stated once");
    let everything = server.ok("search_context", json!({"query": "login_user settings api key database", "project_root": root_arg, "top_k": 20}));
    for leaked in ["abc123secretvalue", "secret-in-dotenv", "ignored-dependency", "settings.py", ".env", "data.bin", "node_modules"] {
        assert!(!everything.contains(leaked), "must not expose {leaked}");
    }
    let overview = server.ok("search_context", json!({"project_root": root_arg}));
    assert!(overview.starts_with("4 of 4 files"), "omitting the query gives a project overview: {overview}");
    let (blocks, is_error) = server.call("search_context", json!({"query": "x", "project_root": "/definitely/not/here"}));
    assert!(is_error && blocks[0].starts_with("Error:"));

    // --- incremental index: only the changed file is embedded again -----------------------------
    assert!(root.join(".cache/context-broker-rs/index.json").is_file());
    write(&root, "src/billing.py", "def charge_card(amount):\n    return gateway.charge(amount)\n\ndef refund_payment(invoice):\n    \"\"\"Return money to the customer.\"\"\"\n    return gateway.refund(invoice)\n");
    let refund = server.ok("search_context", json!({"query": "refund payment money back", "project_root": root_arg, "top_k": 1}));
    assert!(refund.contains("### src/billing.py") && refund.contains("def refund_payment(invoice):"));
    std::fs::remove_file(root.join("docs/weather.md")).unwrap();
    assert!(server.ok("search_context", json!({"query": "rain forecast", "project_root": root_arg})).starts_with("3 of 3 files"));

    // --- literal / regex search -------------------------------------------------------------------
    let literal = server.ok("find_in_codebase", json!({"pattern": "LOGIN_USER", "project_root": root_arg}));
    assert!(literal.starts_with("1 matches in 1/3 files") && literal.contains("### src/auth.py\nL62: def login_user(name, password_hash):"), "{literal}");
    assert!(server.ok("find_in_codebase", json!({"pattern": "LOGIN_USER", "project_root": root_arg, "case_sensitive": true})).starts_with("No matches"));
    let regex = server.ok("find_in_codebase", json!({"pattern": r"def \w+_(card|payment)", "project_root": root_arg, "use_regex": true}));
    assert!(regex.starts_with("2 matches in 1/3 files") && regex.contains("L4: def refund_payment(invoice):"));
    assert!(server.ok("find_in_codebase", json!({"pattern": "def", "project_root": root_arg, "file_glob": "*.toml"})).starts_with("No matches"));
    assert!(server.ok("find_in_codebase", json!({"pattern": "abc123secretvalue", "project_root": root_arg})).starts_with("No matches"));
    assert!(server.call("find_in_codebase", json!({"pattern": "(unclosed", "project_root": root_arg, "use_regex": true})).1);

    // --- routing ----------------------------------------------------------------------------------
    assert!(server.ok("route_task", json!({"task": "grep the exact string login_user"})).starts_with("find_in_codebase"));
    assert!(server.ok("route_task", json!({"task": "save this conversation session"})).contains("record_"));
    assert!(server.ok("route_task", json!({"task": "zzzz"})).starts_with("search_context"));

    // --- chat memory ------------------------------------------------------------------------------
    assert!(server.ok("record_turn", json!({"session_id": "chat one", "project_root": root_arg,
        "user_message": "Redis startup timeout keeps failing on boot", "assistant_message": "Fixed by correcting the Redis port to 6380."})).starts_with("Saved 2 message(s)"));
    assert!(server.ok("record_session", json!({"session_id": "chat-two", "project_root": root_arg, "turns": [
        {"user": "How do we deploy?", "assistant": "Run the release pipeline."},
        {"user": "And rollbacks?", "assistant": "Redeploy the previous tag."}]})).starts_with("Saved 4 message(s)"));
    let (rejected, is_error) = server.call("record_turn", json!({"session_id": "chat one", "project_root": root_arg, "user_message": "PASSWORD=hunter2"}));
    assert!(is_error && rejected[0].contains("secrets"));
    assert!(server.call("record_turn", json!({"session_id": "empty", "project_root": root_arg})).1);

    let session = server.ok("load_chat_context", json!({"session_id": "chat-two", "project_root": root_arg}));
    assert!(session.starts_with("session chat-two · 4 of 4 messages"));
    assert!(session.find("[user] How do we deploy?").unwrap() < session.find("[assistant] Redeploy the previous tag.").unwrap());
    assert!(server.ok("load_chat_context", json!({"session_id": "chat-two", "project_root": root_arg, "limit": 1})).contains("1 of 4 messages"));
    assert!(server.ok("load_chat_context", json!({"session_id": "missing", "project_root": root_arg})).starts_with("No stored messages"));
    assert!(!server.ok("load_chat_context", json!({"session_id": "chat one", "project_root": root_arg})).contains("hunter2"));
    let across = server.ok("load_cross_session_context", json!({"search_query": "redeploy", "project_root": root_arg}));
    assert!(across.starts_with("1 match(es)") && across.contains("[chat-two/assistant] Redeploy the previous tag."));
    assert!(server.call("load_cross_session_context", json!({"search_query": "", "project_root": root_arg})).1);

    // --- project history: explicit lookup and the automatic short note --------------------------
    let history = server.ok("lookup_project_history", json!({"query": "Redis startup timeout", "project_root": root_arg}));
    assert!(history.contains("correcting the Redis port to 6380"));
    assert_eq!(server.ok("lookup_project_history", json!({"query": "avatar cropping", "project_root": root_arg})), "No related project history.");
    let (related, _) = server.call("search_context", json!({"query": "Redis startup timeout", "project_root": root_arg}));
    assert_eq!(related.len(), 2);
    assert!(related[1].starts_with("Related past work") && related[1].contains("Redis port") && related[1].len() < 900);
    assert_eq!(server.call("search_context", json!({"query": "avatar cropping", "project_root": root_arg})).0.len(), 1);

    // --- saved results ----------------------------------------------------------------------------
    assert_eq!(server.ok("list_saved_results", json!({"project_root": root_arg})), "No saved results.");
    assert!(server.ok("save_search_results", json!({"query": "login user", "filename": "../auth notes", "project_root": root_arg})).starts_with("Saved to"));
    assert_eq!(server.ok("list_saved_results", json!({"project_root": root_arg})), "auth notes.json");
    let loaded = server.ok("load_saved_results", json!({"filename": "auth notes", "project_root": root_arg}));
    assert!(loaded.starts_with("query: login user\n") && loaded.contains("### src/auth.py"));
    assert!(server.call("load_saved_results", json!({"filename": "nope", "project_root": root_arg})).1);
    assert!(server.call("no_such_tool", json!({"project_root": root_arg})).1);
    server.shutdown();

    // --- a new process: memory and index persist, nothing is re-embedded --------------------------
    let mut server = Server::start(&storage, &log);
    server.rpc("initialize", json!({"protocolVersion": "2025-06-18"}));
    assert!(server.ok("load_chat_context", json!({"session_id": "chat one", "project_root": root_arg})).contains("Redis port to 6380"));
    assert!(server.ok("search_context", json!({"query": "charge credit card", "project_root": root_arg, "top_k": 1})).contains("### src/billing.py"));
    server.shutdown();

    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(log_text.contains("index ready: 4 files (0 reused, 4 embedded, 1 skipped)"), "{log_text}");
    assert!(log_text.contains("index ready: 4 files (3 reused, 1 embedded, 1 skipped)"), "{log_text}");
    assert!(log_text.contains("index ready: 3 files (3 reused, 0 embedded, 1 skipped)"), "{log_text}");
    assert!(log_text.contains("index loaded from disk (3 files)"), "{log_text}");
    assert!(storage.join("chats").is_dir() && !root.join(".context-broker").exists());
    let _ = std::fs::remove_dir_all(base);
}
