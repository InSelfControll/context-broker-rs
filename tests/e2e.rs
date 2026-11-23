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
        Self::start_with(storage, log, &[])
    }

    /// Runs in the (marker-free) storage folder with only the given settings on top of the defaults.
    fn start_with(storage: &Path, log: &Path, settings: &[(&str, &Path)]) -> Self {
        let log_file = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
        std::fs::create_dir_all(storage).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_context-broker"))
            .current_dir(storage)
            .env("CONTEXT_BROKER_STORAGE_DIR", storage)
            .env_remove("CONTEXT_BROKER_PROJECT_ROOT")
            .env_remove("CONTEXT_BROKER_STORAGE")
            .env_remove("CONTEXT_BROKER_OBSIDIAN_VAULT")
            .env_remove("CONTEXT_BROKER_OBSIDIAN_FOLDER")
            .env_remove("CONTEXT_BROKER_OBSIDIAN_LAYOUT")
            .env_remove("CONTEXT_BROKER_HTTP_ADDR")
            // Nothing installed on the test machine may be detected: no Obsidian settings, empty PATH.
            .env("CONTEXT_BROKER_OBSIDIAN_CONFIG", storage.join("no-obsidian.json"))
            .env("PATH", storage)
            .envs(settings.iter().copied())
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
         "load_cross_session_context", "lookup_project_history", "save_search_results", "list_saved_results", "load_saved_results",
         "export_memory"]
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
    // The same problem in other words, sharing no terms with what was stored, is found by meaning.
    let reworded = server.ok("lookup_project_history", json!({"query": "how to roll back a release", "project_root": root_arg}));
    assert!(reworded.contains("Redeploy the previous tag.") && !reworded.contains("6380"), "{reworded}");
    assert!(server.ok("lookup_project_history", json!({"query": "redis does not start after reboot", "project_root": root_arg})).contains("Redis port to 6380"));
    assert_eq!(server.ok("lookup_project_history", json!({"query": "how do I bake sourdough bread", "project_root": root_arg})), "No related project history.");
    assert!(storage.join("recall").read_dir().unwrap().count() == 1, "exchange embeddings are cached per project");
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
    assert!(log_text.contains("recall: 3 exchange(s), 3 newly embedded") && log_text.contains("recall: 3 exchange(s), 0 newly embedded"), "{log_text}");
    assert!(storage.join("chats").is_dir() && !root.join(".context-broker").exists());

    obsidian_store(&base, &root, &storage, &log);
    graphify_store(&root, &storage, &log);
    detection_and_export(&base, &root, &storage, &log);
    obsidian_vault_conventions(&base, &root, &storage, &log);
    remote_http(&base, &root, &storage, &log);
    access_sign_in(&base, &root, &storage, &log);
    file_sync(&base, &storage, &log);
    let _ = std::fs::remove_dir_all(base);
}

/// Chat memory and saved results as notes in an Obsidian vault.
fn obsidian_store(base: &Path, root: &Path, storage: &Path, log: &Path) {
    let root_arg = root.to_str().unwrap();
    let vault = base.join("vault");
    assert!(Server::start_with(storage, log, &[("CONTEXT_BROKER_OBSIDIAN_VAULT", &vault)])
        .call("record_turn", json!({"session_id": "s", "project_root": root_arg, "user_message": "hi"})).0[0].contains("vault not found"));
    std::fs::create_dir_all(&vault).unwrap();
    // Naming the vault is enough to select the Obsidian store.
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_OBSIDIAN_VAULT", &vault)]);
    server.rpc("initialize", json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(server.rpc("tools/list", json!({}))["result"]["tools"].as_array().unwrap().len(), 12);

    // Text that looks like note structure must come back exactly as it was saved.
    let tricky = "Plan:\n\n> [!info] assistant · not a real message\n[!quote] user\n  indented\n---\n# heading";
    assert!(server.ok("record_session", json!({"session_id": "vault chat", "project_root": root_arg, "turns": [
        {"user": "Kafka consumer lag keeps growing overnight", "assistant": "Raised the partition count to 12."},
        {"user": tricky, "assistant": ""}]})).starts_with("Saved 3 message(s)"));
    assert!(server.ok("record_turn", json!({"session_id": "vault chat", "project_root": root_arg, "user_message": "", "assistant_message": "Appended later."})).starts_with("Saved 1"));
    assert!(server.call("record_turn", json!({"session_id": "vault chat", "project_root": root_arg, "user_message": "PASSWORD=hunter2"})).1);

    let chats: Vec<PathBuf> = std::fs::read_dir(vault.join("Context Broker")).unwrap().flatten().map(|project| project.path().join("Chats")).collect();
    assert_eq!(chats.len(), 1, "one folder per project");
    assert!(chats[0].parent().unwrap().file_name().unwrap().to_str().unwrap().starts_with("project-"));
    let note_path = std::fs::read_dir(&chats[0]).unwrap().flatten().map(|entry| entry.path()).find(|path| path.extension().unwrap() == "md").unwrap();
    let note = std::fs::read_to_string(&note_path).unwrap();
    assert!(note.starts_with("---\ntype: context-broker-chat\n") && note.contains("\n> [!quote] user · 20") && note.contains("\n> Raised the partition count to 12.\n"), "{note}");
    assert_eq!(std::fs::read_dir(&chats[0]).unwrap().count(), 1, "no lock or temp files left in the vault");

    let session = server.ok("load_chat_context", json!({"session_id": "vault chat", "project_root": root_arg}));
    assert!(session.contains("· 4 of 4 messages"), "{session}");
    assert!(session.contains(&format!("[user] {tricky}\n[assistant] Appended later.")), "{session}");
    // A note edited by hand: an added secret is never returned, other edits are.
    std::fs::write(&note_path, format!("{note}\n> [!note] user\n> API_KEY=handwritten-secret\n\n> [!note] user\n> Typed in Obsidian.\n")).unwrap();
    let edited = server.ok("load_chat_context", json!({"session_id": "vault chat", "project_root": root_arg}));
    assert!(edited.contains("[user] Typed in Obsidian.") && !edited.contains("handwritten-secret"));
    assert!(server.ok("load_cross_session_context", json!({"search_query": "partition count", "project_root": root_arg})).starts_with("1 match(es)"));
    assert!(server.ok("lookup_project_history", json!({"query": "Kafka consumer lag", "project_root": root_arg})).contains("partition count to 12"));
    assert!(!server.ok("lookup_project_history", json!({"query": "Redis startup timeout", "project_root": root_arg})).contains("6380"), "stores are separate");

    assert_eq!(server.ok("list_saved_results", json!({"project_root": root_arg})), "No saved results.");
    assert!(server.ok("save_search_results", json!({"query": "charge \"credit\" card", "filename": "billing", "project_root": root_arg})).ends_with("Results/billing.md"));
    assert_eq!(server.ok("list_saved_results", json!({"project_root": root_arg})), "billing.md");
    let loaded = server.ok("load_saved_results", json!({"filename": "billing.md", "project_root": root_arg}));
    assert!(loaded.starts_with("query: charge \"credit\" card\n3 of 3 files") && loaded.contains("### src/billing.py"), "{loaded}");
    server.shutdown();
}

/// Chat memory as Graphify memory docs, plus queries over a Graphify graph.
fn graphify_store(root: &Path, storage: &Path, log: &Path) {
    let root_arg = root.to_str().unwrap();
    let node = |id: &str, label: &str, file: &str| json!({"id": id, "label": label, "source_file": file, "source_location": "L1", "community": 2});
    let link = |source: &str, target: &str, relation: &str, confidence: &str| json!({"source": source, "target": target, "relation": relation, "confidence": confidence});
    write(root, "graphify-out/graph.json", &json!({
        "graph": {"schema_version": 1},
        "nodes": [node("auth_login", "login_user()", "src/auth.py"), node("sessions", "SessionStore", "src/sessions.py"),
                  node("billing_charge", "charge_card()", "src/billing.py"), node("billing_refund", "refund_payment()", "src/billing.py"),
                  node("gateway", "PaymentGateway", "src/gateway.py"), node("island", "Orphan", "")],
        "links": [link("auth_login", "sessions", "calls", "EXTRACTED"), link("billing_charge", "gateway", "calls", "EXTRACTED"),
                  link("billing_refund", "gateway", "calls", "INFERRED"), link("gateway", "sessions", "uses", "EXTRACTED"),
                  link("gateway", "missing_node", "calls", "EXTRACTED")],
    }).to_string());
    // A doc saved by Graphify itself (`graphify save-result`) is part of the project history.
    write(root, "graphify-out/memory/query_20260101_000000_ab12cd34_webhook.md",
        "---\ntype: \"query\"\ndate: \"2026-01-01T00:00:00+00:00\"\nquestion: \"Why do Stripe webhook retries fail?\"\ncontributor: \"graphify\"\n---\n\n# Q: Why do Stripe webhook retries fail?\n\n## Answer\n\nThe signature check used the wrong signing key.\n\n## Source Nodes\n\n- PaymentGateway");

    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_STORAGE", Path::new("graphify"))]);
    server.rpc("initialize", json!({"protocolVersion": "2025-06-18"}));
    let listing = server.rpc("tools/list", json!({}));
    let tools = listing["result"]["tools"].as_array().unwrap();
    assert_eq!((tools.len(), tools[12]["name"].as_str().unwrap()), (13, "query_graph"));
    assert!(server.ok("route_task", json!({"task": "who are the callers and dependencies of this"})).starts_with("query_graph"));

    // --- graph queries ------------------------------------------------------------------------
    let gateway = server.ok("query_graph", json!({"node": "paymentgateway", "project_root": root_arg}));
    assert_eq!(gateway.lines().next().unwrap(), "PaymentGateway · src/gateway.py L1 · community 2 · 3 edges", "dangling edge dropped");
    assert!(gateway.contains("\n--> SessionStore · src/sessions.py L1 [uses]") && gateway.contains("\n<-- refund_payment() · src/billing.py L1 [calls, INFERRED]"), "{gateway}");
    assert!(server.ok("query_graph", json!({"node": "gateway", "project_root": root_arg, "limit": 1})).ends_with("+2 more (raise limit)"));
    let ambiguous = server.ok("query_graph", json!({"node": "_", "project_root": root_arg}));
    assert!(ambiguous.starts_with("3 nodes match '_'; pass one id as node:") && ambiguous.contains("\nbilling_charge — charge_card() · src/billing.py L1"), "{ambiguous}");
    assert!(server.ok("query_graph", json!({"node": "how does the refund work", "project_root": root_arg})).starts_with("refund_payment()"));
    assert_eq!(server.ok("query_graph", json!({"node": "zzz", "project_root": root_arg})), "No node matches 'zzz'.");
    assert_eq!(server.ok("query_graph", json!({"node": "auth_login", "to": "billing_refund", "project_root": root_arg})),
        "3 hop(s): login_user() --calls--> SessionStore <--uses-- PaymentGateway <--calls-- refund_payment()");
    assert_eq!(server.ok("query_graph", json!({"node": "Orphan", "to": "SessionStore", "project_root": root_arg})), "No path between Orphan and SessionStore.");
    // The graph is re-read when Graphify rewrites it.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(root, "graphify-out/graph.json", &json!({"nodes": [node("only", "OnlyNode", "a.py")], "edges": []}).to_string());
    assert_eq!(server.ok("query_graph", json!({"node": "only", "project_root": root_arg})), "OnlyNode · a.py L1 · community 2 · 0 edges");
    std::fs::remove_file(root.join("graphify-out/graph.json")).unwrap();
    assert!(server.call("query_graph", json!({"node": "only", "project_root": root_arg})).0[0].contains("no Graphify graph"));

    // --- memory docs --------------------------------------------------------------------------
    assert!(server.ok("record_session", json!({"session_id": "graph chat", "project_root": root_arg, "turns": [
        {"user": "Nightly export job runs out of memory", "assistant": "Streamed the rows instead of loading them all."},
        {"user": "Second question", "assistant": "Second answer"}]})).starts_with("Saved 4 message(s)"));
    let memory = root.join("graphify-out/memory");
    let docs: Vec<String> = std::fs::read_dir(&memory).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name.starts_with("chat_")).collect();
    assert_eq!(docs.len(), 2, "one doc per exchange: {docs:?}");
    let doc = std::fs::read_to_string(memory.join(docs.iter().find(|name| name.ends_with("nightly_export_job_runs_out_of_memory.md")).unwrap())).unwrap();
    assert!(doc.starts_with("---\ntype: \"chat\"\ndate: \"20") && doc.contains("\nquestion: \"Nightly export job runs out of memory\"\ncontributor: \"context-broker\"\nsession: \"graph-chat-"), "{doc}");
    assert!(doc.ends_with("# Q: Nightly export job runs out of memory\n\n## Answer\n\nStreamed the rows instead of loading them all.\n"));

    let session = server.ok("load_chat_context", json!({"session_id": "graph chat", "project_root": root_arg}));
    assert!(session.contains("· 4 of 4 messages") && session.ends_with("[user] Second question\n[assistant] Second answer"), "{session}");
    let native = server.ok("lookup_project_history", json!({"query": "Stripe webhook retries", "project_root": root_arg}));
    assert!(native.contains("--- graphify\n") && native.contains("wrong signing key") && !native.contains("Source Nodes"), "{native}");
    assert!(server.ok("load_cross_session_context", json!({"search_query": "streamed the rows", "project_root": root_arg})).contains("/assistant] Streamed"));

    assert!(server.ok("save_search_results", json!({"query": "login user", "filename": "auth", "project_root": root_arg})).ends_with("graphify-out/memory/result_auth.md"));
    assert_eq!(server.ok("list_saved_results", json!({"project_root": root_arg})), "auth.md");
    // Graphify's folder is generated output: never indexed, and saved results are not chat history.
    let loaded = server.ok("load_saved_results", json!({"filename": "auth", "project_root": root_arg}));
    assert!(loaded.starts_with("query: login user\n3 of 3 files") && loaded.contains("### src/auth.py"), "{loaded}");
    assert!(!server.ok("load_cross_session_context", json!({"search_query": "login user", "project_root": root_arg})).contains("match(es)"));
    server.shutdown();
}

/// Finding Obsidian and Graphify on the machine, and sending the stored JSON history to them.
fn detection_and_export(base: &Path, root: &Path, storage: &Path, log: &Path) {
    let root_arg = root.to_str().unwrap();
    let dir = |name: &str| {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    };
    let (bare, older, vault, second) = (dir("bare"), dir("vault-older"), dir("vault-open"), dir("second/project"));
    // Obsidian's own settings: the open vault wins over a more recent one; a deleted vault is ignored.
    let settings = base.join("obsidian.json");
    std::fs::write(&settings, json!({"vaults": {
        "a": {"path": older, "ts": 500}, "b": {"path": vault, "ts": 100, "open": true},
        "c": {"path": base.join("deleted-vault"), "ts": 900, "open": true}}}).to_string()).unwrap();
    let detected: &[(&str, &Path)] = &[("CONTEXT_BROKER_OBSIDIAN_CONFIG", &settings)];

    // Nothing to detect: the export says so instead of guessing.
    let mut server = Server::start(storage, log);
    let (nothing, is_error) = server.call("export_memory", json!({"project_root": bare}));
    assert!(is_error && nothing[0].contains("no Obsidian vault or Graphify found"), "{nothing:?}");
    server.shutdown();

    // --- the default JSON store stays in use; its history is sent to the detected vault ---------
    let mut server = Server::start_with(storage, log, detected);
    let exported = server.ok("export_memory", json!({"project_root": root_arg}));
    assert_eq!(exported, format!("Exported to Obsidian vault {}: 6 new message(s) from 2 session(s), 1 new saved result(s), 1 project(s).", vault.display()));
    let project_notes = std::fs::read_dir(vault.join("Context Broker")).unwrap().flatten().next().unwrap().path();
    let note = std::fs::read_to_string(project_notes.join("Chats/chat-two.md")).unwrap();
    assert!(note.contains("\n> [!quote] user · 20") && note.find("> How do we deploy?").unwrap() < note.find("> Redeploy the previous tag.").unwrap(), "{note}");
    assert!(std::fs::read_to_string(project_notes.join("Results/auth notes.md")).unwrap().contains("### src/auth.py"));
    assert!(!older.join("Context Broker").exists());
    // Repeating adds nothing; only messages recorded since are sent.
    assert!(server.ok("export_memory", json!({"project_root": root_arg})).contains(": 0 new message(s) from 2 session(s), 0 new saved result(s)"));
    server.ok("record_turn", json!({"session_id": "chat-two", "project_root": root_arg, "user_message": "Recorded after the first export."}));
    assert!(server.ok("export_memory", json!({"project_root": root_arg, "target": "obsidian"})).contains(": 1 new message(s) from 2 session(s)"));
    assert_eq!(std::fs::read_to_string(project_notes.join("Chats/chat-two.md")).unwrap().matches("> [!").count(), 5);
    // Every stored project at once; one whose folder is gone is reported, not guessed.
    let gone = dir("gone");
    for project in [&second, &gone] {
        server.ok("record_turn", json!({"session_id": "other", "project_root": project, "user_message": "Belongs to another project."}));
    }
    std::fs::remove_dir(&gone).unwrap();
    let everything = server.ok("export_memory", json!({"project_root": root_arg, "all_projects": true}));
    assert!(everything.contains(": 1 new message(s) from 3 session(s), 0 new saved result(s), 2 project(s). 1 stored project(s) skipped"), "{everything}");
    assert!(std::fs::read_dir(vault.join("Context Broker")).unwrap().flatten().any(|entry| entry.file_name().to_string_lossy().starts_with("project-") && entry.path().join("Chats/other.md").is_file()));

    // --- the same history into Graphify memory docs ---------------------------------------------
    let memory = root.join("graphify-out/memory");
    let docs = || std::fs::read_dir(&memory).unwrap().count();
    let before = docs();
    let to_graph = server.ok("export_memory", json!({"project_root": root_arg, "target": "graphify"}));
    assert!(to_graph.starts_with("Exported to Graphify memory (graphify-out/memory): 7 new message(s) from 2 session(s), 1 new saved result(s)"), "{to_graph}");
    assert_eq!(docs() - before, 5, "four exchanges and one saved result");
    assert!(server.ok("export_memory", json!({"project_root": root_arg, "target": "graphify"})).contains(": 0 new message(s)"));
    assert_eq!(docs() - before, 5);
    assert!(server.call("export_memory", json!({"project_root": root_arg, "target": "sqlite"})).1);
    server.shutdown();
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_STORAGE", Path::new("graphify"))]);
    let sent = server.ok("load_chat_context", json!({"session_id": "chat-two", "project_root": root_arg}));
    assert!(sent.contains("· 5 of 5 messages") && sent.contains("[user] How do we deploy?\n[assistant] Run the release pipeline.") && sent.ends_with("[user] Recorded after the first export."), "{sent}");
    assert!(server.ok("load_chat_context", json!({"session_id": "chat one", "project_root": root_arg})).contains("Redis port to 6380"));
    server.shutdown();

    // --- CONTEXT_BROKER_STORAGE=auto ------------------------------------------------------------
    let auto: (&str, &Path) = ("CONTEXT_BROKER_STORAGE", Path::new("auto"));
    let record = |server: &mut Server, project: &Path| server.ok("record_turn", json!({"session_id": "auto", "project_root": project, "user_message": "Where does this go?"}));
    // A detected vault comes first.
    let mut server = Server::start_with(storage, log, &[auto, detected[0]]);
    record(&mut server, &bare);
    assert!(std::fs::read_dir(vault.join("Context Broker")).unwrap().flatten().any(|entry| entry.path().join("Chats/auto.md").is_file()));
    server.shutdown();
    // No vault, but the Graphify CLI is installed.
    let tools_dir = dir("bin");
    std::fs::write(tools_dir.join("graphify"), "").unwrap();
    let mut server = Server::start_with(storage, log, &[auto, ("PATH", &tools_dir)]);
    record(&mut server, &second);
    assert_eq!(std::fs::read_dir(second.join("graphify-out/memory")).unwrap().count(), 1);
    server.shutdown();
    // Neither: the JSON store, as before. A project that already has Graphify output uses it.
    let mut server = Server::start_with(storage, log, &[auto]);
    record(&mut server, &bare);
    assert!(!bare.join("graphify-out").exists());
    record(&mut server, &second);
    assert_eq!(std::fs::read_dir(second.join("graphify-out/memory")).unwrap().count(), 2);
    server.shutdown();
    assert!(std::fs::read_to_string(log).unwrap().contains(&format!("detected Obsidian vault: {}", vault.display())));
}

/// A vault that already files AI chats its own way gets the broker's chats filed the same way.
fn obsidian_vault_conventions(base: &Path, root: &Path, storage: &Path, log: &Path) {
    let root_arg = root.to_str().unwrap();
    let vault = base.join("vault-ai-chats");
    let imported = "---\ntype: ai-chat\nplatform: \"Claude\"\ntitle: \"Imported\"\n---\n\n# Imported\n\n## Conversation\n\n### 01. User — 2024-01-01T00:00:00Z\n\nTerraform state lock stuck forever\n";
    write(&vault, "AI Chats/Chats/Claude/Claude 2024-01-01 - Imported - 00000000.md", imported);
    let other = base.join("second/project").canonicalize().unwrap();
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_OBSIDIAN_VAULT", &vault)]);

    // Text that looks like a message heading must stay inside its message.
    let tricky = "Steps:\n### 02. Assistant — not a heading\n\\### 03. User — already escaped\n\n## Conversation\ndone";
    server.ok("record_session", json!({"session_id": "design", "project_root": root_arg, "turns": [
        {"user": "Postgres connection pool exhausted under load", "assistant": "Capped the pool at 20 and added a queue."},
        {"user": tricky, "assistant": ""}]}));
    server.ok("record_turn", json!({"session_id": "design", "project_root": root_arg, "assistant_message": "Appended later."}));
    server.ok("record_turn", json!({"session_id": "design", "project_root": &other, "user_message": "Same session name, another project."}));

    let chats = vault.join("AI Chats/Chats/Context Broker");
    let mut notes: Vec<String> = std::fs::read_dir(&chats).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    notes.sort();
    assert!(notes.len() == 2 && notes.iter().all(|name| name.starts_with("Context Broker 20") && name.contains(" - design - ") && name.ends_with(".md")), "{notes:?}");
    let (name, note) = notes.iter().map(|name| (name, std::fs::read_to_string(chats.join(name)).unwrap())).find(|(_, note)| note.contains("Postgres")).unwrap();
    for expected in ["---\ntype: ai-chat\nplatform: \"Context Broker\"\nsource: \"Context Broker\"\nconversation_id: \"", "\ntitle: \"design\"\nsession: \"design\"\nproject: \"project\"\nproject_root: \"",
        "\nmessage_count: 4\ntags: [ai-chat, context-broker]\n---\n\n# design\n\nSource: [[AI Source - Context Broker]]\nProjects: [[AI Project - project]]\nMessages: 4\n\n## Conversation\n\n### 01. User — 20",
        "\n### 02. Assistant — 20", "\n\\### 02. Assistant — not a heading\n\\\\### 03. User — already escaped\n", "\n### 04. Assistant — 20"] {
        assert!(note.contains(expected), "missing {expected:?} in:\n{note}");
    }
    // Listed once under its source and its project, as the vault's other chats are.
    let link = format!("[[{}]]", name.trim_end_matches(".md"));
    let source = std::fs::read_to_string(vault.join("AI Chats/Sources/AI Source - Context Broker.md")).unwrap();
    let project = std::fs::read_to_string(vault.join("AI Chats/Projects/AI Project - project.md")).unwrap();
    assert!(source.starts_with("---\ntype: ai-chat-source\n") && source.matches("[[Context Broker 20").count() == 2 && source.matches(&link).count() == 1, "{source}");
    assert!(project.starts_with("---\ntype: ai-project\n") && project.contains("## Connected chats") && project.matches("[[Context Broker 20").count() == 2, "{project}");
    assert!(!vault.join("Context Broker").exists(), "nothing outside the vault's own structure");

    let session = server.ok("load_chat_context", json!({"session_id": "design", "project_root": root_arg}));
    assert!(session.contains("· 4 of 4 messages") && session.contains(&format!("[user] {tricky}\n[assistant] Appended later.")), "{session}");
    assert!(server.ok("load_chat_context", json!({"session_id": "design", "project_root": &other})).contains("· 1 of 1 messages"));
    assert!(server.ok("lookup_project_history", json!({"query": "Postgres connection pool exhausted", "project_root": root_arg})).contains("Capped the pool at 20"));
    // Chats imported from other platforms are the vault's own and are not read as broker history.
    assert_eq!(server.ok("lookup_project_history", json!({"query": "Terraform state lock stuck", "project_root": root_arg})), "No related project history.");
    assert!(server.ok("save_search_results", json!({"query": "login user", "filename": "auth", "project_root": root_arg})).contains("/AI Chats/Results/project-"));
    assert_eq!(server.ok("list_saved_results", json!({"project_root": root_arg})), "auth.md");
    // The stored JSON history lands in the same structure, once.
    assert!(server.ok("export_memory", json!({"project_root": root_arg})).contains(": 7 new message(s) from 2 session(s), 1 new saved result(s)"));
    assert!(server.ok("export_memory", json!({"project_root": root_arg})).contains(": 0 new message(s) from 2 session(s), 0 new saved result(s)"));
    assert!(server.ok("load_chat_context", json!({"session_id": "chat-two", "project_root": root_arg})).contains("· 5 of 5 messages"));
    assert_eq!(std::fs::read_to_string(chats.parent().unwrap().join("Claude/Claude 2024-01-01 - Imported - 00000000.md")).unwrap(), imported);
    server.shutdown();

    // The broker's own layout can still be asked for, and follows Obsidian's folder for new notes.
    write(&vault, ".obsidian/app.json", r#"{"newFileLocation": "folder", "newFileFolderPath": "Inbox/Notes"}"#);
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_OBSIDIAN_VAULT", &vault), ("CONTEXT_BROKER_OBSIDIAN_LAYOUT", Path::new("broker"))]);
    server.ok("record_turn", json!({"session_id": "own", "project_root": root_arg, "user_message": "Own layout."}));
    assert!(std::fs::read_dir(vault.join("Inbox/Notes/Context Broker")).unwrap().flatten().any(|entry| entry.path().join("Chats/own.md").is_file()));
    server.shutdown();
}

/// The HTTP transport for remote clients: same tools, confined to one project.
fn remote_http(base: &Path, root: &Path, storage: &Path, log: &Path) {
    use std::io::Read;
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let address = format!("127.0.0.1:{port}");
    // Without a project to confine it to, the server refuses to start.
    let mut unconfined = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&address))]);
    assert!(!unconfined.child.wait().unwrap().success());
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&address)), ("CONTEXT_BROKER_PROJECT_ROOT", root)]);
    let http = |method: &str, path: &str, body: &str| -> (u16, String) {
        let mut stream = (0..100)
            .find_map(|_| std::net::TcpStream::connect(&address).ok().or_else(|| { std::thread::sleep(std::time::Duration::from_millis(50)); None }))
            .expect("server is listening");
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let (head, text) = reply.split_once("\r\n\r\n").unwrap();
        (head.split_whitespace().nth(1).unwrap().parse().unwrap(), text.to_string())
    };
    let rpc = |method: &str, params: Value| -> Value {
        let (status, text) = http("POST", "/mcp", &json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).to_string());
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).unwrap()
    };
    let call = |name: &str, args: Value| -> (String, bool) {
        let reply = rpc("tools/call", json!({"name": name, "arguments": args}));
        (reply["result"]["content"][0]["text"].as_str().unwrap().to_string(), reply["result"]["isError"].as_bool().unwrap())
    };

    assert_eq!(rpc("initialize", json!({"protocolVersion": "2025-06-18"}))["result"]["serverInfo"]["name"], "context-broker");
    assert_eq!(http("POST", "/mcp", r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).0, 202);
    assert_eq!((http("GET", "/mcp", "").0, http("POST", "/elsewhere", "{}").0), (405, 404));
    let listing = rpc("tools/list", json!({}));
    let names: Vec<&str> = listing["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"read_file") && names.contains(&"search_context"), "{names:?}");

    // The project is the configured one; a caller cannot point the server anywhere else.
    assert!(call("search_context", json!({"query": "charge credit card", "top_k": 1})).0.contains("### src/billing.py"));
    for outside in [base.to_str().unwrap(), "/etc", "../.."] {
        let (text, is_error) = call("search_context", json!({"query": "x", "project_root": outside}));
        assert!(is_error && text.contains("project_root must be inside"), "{text}");
    }
    let (file, _) = call("read_file", json!({"path": "src/billing.py", "start_line": 4, "max_lines": 2}));
    assert_eq!(file, "src/billing.py · lines 4-5 of 6\n4: def refund_payment(invoice):\n5:     \"\"\"Return money to the customer.\"\"\"");
    for blocked in ["../storage/no-obsidian.json", "/etc/hostname", ".env", "src/settings.py", "src/missing.py"] {
        let (text, is_error) = call("read_file", json!({"path": blocked}));
        assert!(is_error && !text.contains("secret-in-dotenv") && !text.contains("abc123secretvalue"), "{blocked}: {text}");
    }
    // Version-control internals (remote URLs with tokens) and well-known key formats stay hidden.
    let token = format!("ghp_{}", "a1".repeat(18));
    write(root, ".git/config", "[remote \"origin\"]\n\turl = https://example.com/repo.git\n");
    write(root, "src/client.js", &format!("const client = connect({{\"auth\":\"{token}\"}});\n"));
    for blocked in [".git/config", "src/client.js"] {
        let (text, is_error) = call("read_file", json!({"path": blocked}));
        assert!(is_error && !text.contains("example.com") && !text.contains(&token), "{blocked}: {text}");
    }
    // Nor by naming the private folder itself as the project root.
    for (sub_root, path) in [(".git", "config"), ("./.git", "config"), ("src/../.git", "config")] {
        let (text, is_error) = call("read_file", json!({"project_root": sub_root, "path": path}));
        assert!(is_error && !text.contains("example.com"), "{sub_root}/{path}: {text}");
    }
    let (text, is_error) = call("search_context", json!({"query": "remote origin", "project_root": ".git"}));
    assert!(is_error && text.contains("hidden folder"), "{text}");
    let (_, is_error) = call("read_file", json!({"project_root": "src", "path": "billing.py"}));
    assert!(!is_error, "an ordinary sub-folder still works as a root");
    std::fs::remove_dir_all(root.join(".git")).unwrap();
    std::fs::remove_file(root.join("src/client.js")).unwrap();
    // Exporting every project's memory would reach past the served folder.
    let (text, is_error) = call("export_memory", json!({"target": "obsidian", "all_projects": true}));
    assert!(is_error && text.contains("all_projects"), "{text}");

    // Browsers: a page from another site (CSRF) or a DNS name pointed at this machine (DNS
    // rebinding) is refused; the Obsidian apps and local pages are not.
    let status_with = |extra: &str| -> u16 {
        let mut stream = std::net::TcpStream::connect(&address).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        write!(stream, "POST /mcp HTTP/1.1\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    let host = format!("Host: {address}\r\n");
    for (extra, expected) in [
        (host.clone(), 200), ("Host: localhost:8765\r\n".to_string(), 200), ("Host: [::1]:8765\r\n".to_string(), 200),
        ("Host: rebind.attacker.example:8765\r\n".to_string(), 403),
        (format!("{host}Origin: https://attacker.example\r\n"), 403), (format!("{host}Origin: null\r\n"), 403),
        (format!("{host}Origin: http://localhost.attacker.example\r\n"), 403),
        (format!("{host}Origin: http://localhost:3000\r\n"), 200), (format!("{host}Origin: app://obsidian.md\r\n"), 200),
        (format!("{host}Origin: capacitor://localhost\r\n"), 200),
    ] {
        assert_eq!(status_with(&extra), expected, "{extra:?}");
    }
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    drop(server.stdin.take());

    // Open to every caller only on loopback: a non-loopback bind with no allow-list or sign-in
    // refuses to start.
    let wide = format!("0.0.0.0:{}", std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port());
    let mut open = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&wide)), ("CONTEXT_BROKER_PROJECT_ROOT", root)]);
    assert!(!open.child.wait().unwrap().success());
    drop(open.stdin.take());

    // A named host is answered once listed.
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&address)), ("CONTEXT_BROKER_PROJECT_ROOT", root),
        ("CONTEXT_BROKER_HTTP_ALLOW_HOSTS", Path::new("broker.example.org"))]);
    let listed_host = || -> u16 {
        let mut stream = (0..100)
            .find_map(|_| std::net::TcpStream::connect(&address).ok().or_else(|| { std::thread::sleep(std::time::Duration::from_millis(50)); None }))
            .expect("server is listening");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: Broker.Example.org\r\nOrigin: https://broker.example.org\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    assert_eq!(listed_host(), 200);

    server.child.kill().unwrap();
    server.child.wait().unwrap();
    drop(server.stdin.take());

    // An allow-list of caller addresses (as reported by the proxy in front) refuses everyone else.
    let second = format!("127.0.0.1:{}", std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port());
    let both = format!("{address}, {second}");
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&both)), ("CONTEXT_BROKER_PROJECT_ROOT", root),
        ("CONTEXT_BROKER_HTTP_ALLOW_IPS", Path::new("203.0.113.0/24, 2001:db8:aa00::/40,198.51.100.7"))]);
    let status_at = |address: &str, caller: &str| -> u16 {
        let mut stream = (0..100)
            .find_map(|_| std::net::TcpStream::connect(address).ok().or_else(|| { std::thread::sleep(std::time::Duration::from_millis(50)); None }))
            .expect("server is listening");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let header = if caller.is_empty() { String::new() } else { format!("CF-Connecting-IP: {caller}\r\n") };
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: {address}\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    for (caller, expected) in [("203.0.113.9", 200), ("198.51.100.7", 200), ("2001:db8:aaff::1", 200), ("", 403), ("203.0.114.9", 403), ("198.51.100.8", 403), ("2001:db8:ab00::1", 403), ("not-an-address", 403)] {
        assert_eq!(status_at(&address, caller), expected, "caller {caller:?}");
    }
    // Every listen address serves, with the same rules.
    assert_eq!((status_at(&second, "203.0.113.9"), status_at(&second, "")), (200, 403));
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    drop(server.stdin.take());
}

/// Callers signed in through Cloudflare Access are accepted by their token, whatever their address.
fn access_sign_in(base: &Path, root: &Path, storage: &Path, log: &Path) {
    use std::io::Read;
    // A throwaway signing key, and a stand-in for the team's public-key endpoint.
    let key_file = base.join("access-test-key.pem");
    assert!(Command::new("openssl").args(["genrsa", "-out"]).arg(&key_file).arg("2048").stderr(Stdio::null()).status().unwrap().success());
    let modulus = String::from_utf8(Command::new("openssl").args(["rsa", "-noout", "-modulus", "-in"]).arg(&key_file).output().unwrap().stdout).unwrap();
    let hex = modulus.trim().trim_start_matches("Modulus=");
    let bytes: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut n = String::new();
    for chunk in bytes.chunks(3) {
        let word = chunk.iter().fold(0u32, |acc, byte| acc << 8 | u32::from(*byte)) << (8 * (3 - chunk.len()));
        for position in 0..=chunk.len() {
            n.push(alphabet[(word >> (18 - 6 * position) & 63) as usize] as char);
        }
    }
    let certs = json!({"keys": [{"kid": "k1", "kty": "RSA", "alg": "RS256", "n": n, "e": "AQAB"}]}).to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let certs_url = format!("http://{}/cdn-cgi/access/certs", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.read(&mut [0u8; 2048]);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{certs}", certs.len());
        }
    });

    let signing = jsonwebtoken::EncodingKey::from_rsa_pem(&std::fs::read(&key_file).unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let token = |issuer: &str, audience: &str, expires: u64, key_id: &str| {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(key_id.to_string());
        jsonwebtoken::encode(&header, &json!({"iss": issuer, "aud": [audience], "exp": expires, "email": "owner@example.com"}), &signing).unwrap()
    };

    let address = format!("127.0.0.1:{}", std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port());
    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&address)), ("CONTEXT_BROKER_PROJECT_ROOT", root),
        ("CONTEXT_BROKER_HTTP_ALLOW_IPS", Path::new("203.0.113.0/24")), ("CONTEXT_BROKER_ACCESS_TEAM_DOMAIN", Path::new("team.example")),
        ("CONTEXT_BROKER_ACCESS_AUD", Path::new("app-audience")), ("CONTEXT_BROKER_ACCESS_CERTS_URL", Path::new(&certs_url))]);
    let status = |caller: &str, token: &str| -> u16 {
        let mut stream = (0..100)
            .find_map(|_| std::net::TcpStream::connect(&address).ok().or_else(|| { std::thread::sleep(std::time::Duration::from_millis(50)); None }))
            .expect("server is listening");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let sign_in = if token.is_empty() { String::new() } else { format!("Cf-Access-Jwt-Assertion: {token}\r\n") };
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: {address}\r\nCF-Connecting-IP: {caller}\r\n{sign_in}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    let outsider = "198.51.100.50";
    let valid = token("https://team.example", "app-audience", now + 600, "k1");
    assert_eq!(status(outsider, &valid), 200, "a signed-in caller from any address");
    assert_eq!(status("203.0.113.9", ""), 200, "a listed address needs no sign-in");
    assert_eq!(status(outsider, ""), 403, "an unknown address without sign-in");
    for (why, forged) in [("another application's token", token("https://team.example", "other-app", now + 600, "k1")),
        ("another team's token", token("https://evil.example", "app-audience", now + 600, "k1")),
        ("an expired token", token("https://team.example", "app-audience", now - 600, "k1")),
        ("an unknown signing key", token("https://team.example", "app-audience", now + 600, "k2")),
        ("a tampered token", format!("{}x", valid)), ("not a token", "garbage".to_string())] {
        assert_eq!(status(outsider, &forged), 403, "{why}");
    }
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    drop(server.stdin.take());
}

/// The file-sync endpoints a remote copy of the folder (the Obsidian plugin) talks to.
fn file_sync(base: &Path, storage: &Path, log: &Path) {
    use std::io::Read;
    let vault = base.join("sync-vault");
    write(&vault, "Existing.md", "already here\n");
    write(&vault, ".obsidian/app.json", "{}");
    write(&vault, "Folder/.hidden.md", "hidden");
    write(&vault, "Note.lock", "scratch");
    let vault = vault.canonicalize().unwrap();
    let address = format!("127.0.0.1:{}", std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port());
    let http = |method: &str, target: &str, base_hash: Option<&str>, body: &[u8]| -> (u16, String, Vec<u8>) {
        let mut stream = (0..100)
            .find_map(|_| std::net::TcpStream::connect(&address).ok().or_else(|| { std::thread::sleep(std::time::Duration::from_millis(50)); None }))
            .expect("server is listening");
        let guard = base_hash.map(|hash| format!("X-Base-Hash: {hash}\r\n")).unwrap_or_default();
        write!(stream, "{method} {target} HTTP/1.1\r\nHost: {address}\r\n{guard}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(body).unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        let split = reply.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&reply[..split]).into_owned();
        let mut body = reply[split + 4..].to_vec();
        if head.to_lowercase().contains("transfer-encoding: chunked") {
            // Large replies arrive in chunks: "<hex length>\r\n<bytes>\r\n", ended by a zero length.
            let (mut rest, mut whole) = (body.as_slice(), Vec::new());
            while let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") {
                let length = usize::from_str_radix(std::str::from_utf8(&rest[..line_end]).unwrap().trim(), 16).unwrap();
                if length == 0 {
                    break;
                }
                whole.extend_from_slice(&rest[line_end + 2..line_end + 2 + length]);
                rest = &rest[line_end + 4 + length..];
            }
            body = whole;
        }
        (head.split_whitespace().nth(1).unwrap().parse().unwrap(), head, body)
    };
    let json_of = |body: &[u8]| -> Value { serde_json::from_slice(body).unwrap() };

    // Off unless asked for: without CONTEXT_BROKER_SYNC the routes do not exist.
    let mut plain = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&address)), ("CONTEXT_BROKER_PROJECT_ROOT", &vault)]);
    assert_eq!(http("GET", "/sync/manifest", None, b"").0, 404);
    plain.child.kill().unwrap();
    plain.child.wait().unwrap();
    drop(plain.stdin.take());

    let mut server = Server::start_with(storage, log, &[("CONTEXT_BROKER_HTTP_ADDR", Path::new(&address)), ("CONTEXT_BROKER_PROJECT_ROOT", &vault), ("CONTEXT_BROKER_SYNC", Path::new("1"))]);
    let manifest = || -> Vec<(String, String)> {
        let (status, _, body) = http("GET", "/sync/manifest", None, b"");
        assert_eq!(status, 200);
        json_of(&body)["files"].as_array().unwrap().iter().map(|file| (file["path"].as_str().unwrap().to_string(), file["hash"].as_str().unwrap().to_string())).collect()
    };
    // Hidden files and folders and the broker's scratch files are never listed.
    let listed = manifest();
    assert_eq!(listed.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(), ["Existing.md"]);
    let existing_hash = listed[0].1.clone();
    assert_eq!(existing_hash.len(), 64);

    // Read; create in a new folder (a name with spaces and non-ASCII letters); binary content.
    let (status, head, body) = http("GET", "/sync/file?path=Existing.md", None, b"");
    assert!(status == 200 && body == b"already here\n" && head.to_lowercase().contains(&format!("x-content-hash: {existing_hash}")), "{head}");
    assert_eq!(http("GET", "/sync/file?path=Missing.md", None, b"").0, 404);
    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let (status, _, body) = http("PUT", "/sync/file?path=New%20Folder/%D7%A9%D7%9C%D7%95%D7%9D+pic.bin", Some(""), &binary);
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(std::fs::read(vault.join("New Folder/שלום pic.bin")).unwrap(), binary);
    assert_eq!(http("GET", "/sync/file?path=New%20Folder/%D7%A9%D7%9C%D7%95%D7%9D%20pic.bin", None, b"").2, binary);

    // A write must name what it replaces: a stale base is refused and reports the current hash.
    let (status, _, body) = http("PUT", "/sync/file?path=Existing.md", Some(&existing_hash), b"edited on device A\n");
    let hash_a = json_of(&body)["hash"].as_str().unwrap().to_string();
    assert!(status == 200 && hash_a != existing_hash);
    let (status, _, body) = http("PUT", "/sync/file?path=Existing.md", Some(&existing_hash), b"edited on device B\n");
    assert!(status == 409 && json_of(&body)["hash"] == hash_a.as_str(), "the second device learns it is behind");
    assert_eq!(std::fs::read(vault.join("Existing.md")).unwrap(), b"edited on device A\n");
    assert_eq!(http("PUT", "/sync/file?path=Existing.md", Some(""), b"edited on device A\n").0, 200, "sending what is already there is fine");
    assert_eq!(http("PUT", "/sync/file?path=Existing.md", Some(""), b"new file claim\n").0, 409, "it does exist");

    // Deletes are guarded the same way.
    assert_eq!(http("DELETE", "/sync/file?path=Existing.md", Some(&existing_hash), b"").0, 409);
    assert!(vault.join("Existing.md").exists());
    assert_eq!(http("DELETE", "/sync/file?path=Existing.md", Some(&hash_a), b"").0, 200);
    assert!(!vault.join("Existing.md").exists());
    assert_eq!(http("DELETE", "/sync/file?path=Existing.md", Some(&hash_a), b"").0, 200, "already gone");

    // Nothing hidden, outside the folder, or scratch can be read, written or deleted.
    for bad in [".obsidian/app.json", "Folder/.hidden.md", "../storage/x", "/etc/hostname", "a//b.md", "Note.lock", "..%2F..%2Fetc%2Fpasswd", ""] {
        for (method, guard) in [("GET", None), ("PUT", Some("")), ("DELETE", Some(""))] {
            let (status, _, _) = http(method, &format!("/sync/file?path={bad}"), guard, b"x");
            assert_eq!(status, 400, "{method} {bad:?}");
        }
    }
    assert_eq!(std::fs::read_to_string(vault.join(".obsidian/app.json")).unwrap(), "{}");
    // A symlinked folder inside the vault does not lead out of it.
    let outside = base.join("sync-outside");
    write(&outside, "private.md", "outside the vault\n");
    std::os::unix::fs::symlink(&outside, vault.join("Linked")).unwrap();
    let outside_hash = hex_sha256(b"outside the vault\n");
    for (method, path, guard) in [("GET", "Linked/private.md", None), ("DELETE", "Linked/private.md", Some(outside_hash.as_str())),
        ("PUT", "Linked/private.md", Some(outside_hash.as_str())), ("PUT", "Linked/new/created.md", Some(""))] {
        assert_eq!(http(method, &format!("/sync/file?path={path}"), guard, b"overwritten").0, 400, "{method} {path}");
    }
    assert_eq!(std::fs::read_to_string(outside.join("private.md")).unwrap(), "outside the vault\n");
    assert!(!outside.join("new").exists());
    std::fs::remove_file(vault.join("Linked")).unwrap();
    let after: Vec<String> = manifest().into_iter().map(|(path, _)| path).collect();
    assert_eq!(after, ["New Folder/שלום pic.bin"]);
    assert_eq!(http("POST", "/sync/file?path=x.md", None, b"").0, 404);
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    drop(server.stdin.take());
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
