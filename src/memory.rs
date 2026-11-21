//! Persistence for chat memory, history lookup and saved search results.
//!
//! Three stores, chosen by `CONTEXT_BROKER_STORAGE`:
//! - `json` (default): `<storage>/chats/<project_digest>/<session>.json`, the Python broker's
//!   schema, so existing history stays readable.
//! - `obsidian`: Markdown notes in a vault, one note per session, every message a callout.
//! - `graphify`: Graphify memory docs in `<project>/graphify-out/memory/`, one per exchange, which
//!   Graphify folds into its knowledge graph on the next update.

use crate::config::{self, AI_CHATS, GRAPHIFY_OUT, IN_PROJECT_FOLDER, Store};
use crate::index::hex;
use crate::project::project_name;
use crate::security;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_HISTORY_FILES: usize = 128;
/// Graphify keeps one file per exchange, so its window is wider.
const MAX_MEMORY_DOCS: usize = 512;
const MAX_HISTORY_FILE_BYTES: u64 = 1_000_000;
const MAX_EXCERPT_CHARS: usize = 2_000;
/// Newest exchanges compared by meaning in a history lookup.
const RECALL_CANDIDATES: usize = 300;
/// Cosine similarity from which an exchange counts as the same topic as the query. Measured with
/// all-MiniLM-L6-v2: unrelated questions score below 0.25, reworded ones from about 0.4 up.
const RECALL_MIN_SCORE: f32 = 0.35;
const HISTORY_STOPWORDS: &str = "a an the is are was were be been to of for in on at with and or it this that \
my our your we you i me can could should would please how what why when where \
do does did has have had make sure fix issue problem help again";
const ANSWER_HEADING: &str = "\n\n## Answer\n\n";
/// The name this server's notes carry in a vault that files AI chats by platform.
const PLATFORM: &str = "Context Broker";
const CONVERSATION_HEADING: &str = "\n\n## Conversation\n";

/// One stored conversation: `(peer, content)` messages, oldest first.
struct Session {
    name: String,
    source: String,
    messages: Vec<(String, String)>,
}

fn sha256_hex(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

fn project_digest(root: &Path) -> String {
    sha256_hex(&root.to_string_lossy())[..16].to_string()
}

/// Keep `[alnum - _ .]`; when that is lossy, append a digest so distinct ids never collide.
fn safe_id(value: &str) -> String {
    let candidate = if value.trim().is_empty() { "default" } else { value.trim() };
    let normalized: String =
        candidate.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    if normalized == candidate { normalized } else { format!("{normalized}-{}", &sha256_hex(candidate)[..10]) }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// UTC as `2026-11-21T06:00:00Z` and as the sortable `20261121_060000`.
fn utc(stamp: f64) -> (String, String) {
    let secs = stamp as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since the epoch to a calendar date (Howard Hinnant's civil_from_days).
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted.div_euclid(146_097), shifted.rem_euclid(146_097));
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
    (
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"),
        format!("{year:04}{month:02}{day:02}_{h:02}{m:02}{s:02}"),
    )
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Serialize read-modify-write across broker processes with a lock file.
fn with_lock<T>(path: &Path, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let lock = path.with_extension("lock");
    let started = Instant::now();
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => break,
            Err(_) if started.elapsed() > Duration::from_secs(10) => {
                // A lock this old belongs to a process that died mid-write.
                let _ = std::fs::remove_file(&lock);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let result = action();
    let _ = std::fs::remove_file(&lock);
    result
}

fn chats_dir(root: &Path) -> PathBuf {
    config::storage_dir().join("chats").join(project_digest(root))
}

/// `<vault>/<folder>/<project>-<digest>/<kind>`: one folder per project, readable in Obsidian.
fn obsidian_dir(vault: &Path, root: &Path, kind: &str) -> PathBuf {
    config::obsidian_new_notes(vault)
        .join(config::obsidian_folder())
        .join(format!("{}-{}", project_name(root), &project_digest(root)[..8]))
        .join(kind)
}

fn graphify_memory(root: &Path) -> PathBuf {
    root.join(GRAPHIFY_OUT).join("memory")
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A one-line, double-quoted YAML scalar.
fn yaml_quote(text: &str) -> String {
    format!("\"{}\"", clip(text, 300).replace('\\', "\\\\").replace('"', "\\\""))
}

fn yaml_unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next().unwrap_or('\\') } else { c });
    }
    out
}

/// Split a Markdown document into its YAML front matter (flat `key: value` lines) and body.
fn front_matter(text: &str) -> (HashMap<String, String>, &str) {
    let mut meta = HashMap::new();
    let Some((head, body)) = text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---\n")) else {
        return (meta, text);
    };
    for line in head.lines() {
        if let Some((key, value)) = line.split_once(':') {
            meta.insert(key.trim().to_string(), yaml_unquote(value.trim()));
        }
    }
    (meta, body)
}

/// One `(time, user, assistant)` exchange; either side may be empty.
type Exchange<'a> = (f64, &'a str, &'a str);

/// The non-empty messages of exchanges, in order, as `(peer, content, time)`.
fn spoken<'a>(exchanges: &'a [Exchange<'a>]) -> impl Iterator<Item = (&'static str, &'a str, f64)> {
    exchanges
        .iter()
        .flat_map(|(stamp, user, assistant)| [("user", *user, *stamp), ("assistant", *assistant, *stamp)])
        .filter(|(_, content, _)| !content.trim().is_empty())
}

/// Append `(user, assistant)` turns to one session; prior turns are never overwritten.
pub fn record(root: &Path, session_id: &str, turns: &[(String, String)]) -> Result<String, String> {
    let session = safe_id(session_id);
    let stamp = now();
    let exchanges: Vec<Exchange> = turns.iter().map(|(user, assistant)| (stamp, user.as_str(), assistant.as_str())).collect();
    if spoken(&exchanges).any(|(_, content, _)| security::has_secret_content(content)) {
        return Err("message contains potential secrets; nothing was saved".into());
    }
    let saved = spoken(&exchanges).count();
    if saved == 0 {
        return Err("nothing to record: every message is empty".into());
    }
    write(&config::store(root)?, root, &session, &exchanges)?;
    Ok(format!("Saved {saved} message(s) to session '{session}'."))
}

/// Append exchanges to a session of `store`; returns how many messages were written.
fn write(store: &Store, root: &Path, session: &str, exchanges: &[Exchange]) -> Result<usize, String> {
    match store {
        Store::Json => record_json(root, session, exchanges),
        Store::Obsidian(vault) => record_obsidian(vault, root, session, exchanges),
        Store::Graphify => record_graphify(root, session, exchanges),
    }
}

fn record_json(root: &Path, session: &str, exchanges: &[Exchange]) -> Result<usize, String> {
    let path = chats_dir(root).join(format!("{session}.json"));
    std::fs::create_dir_all(path.parent().expect("chats dir")).map_err(|e| e.to_string())?;
    with_lock(&path, || {
        let mut ledger = read_json(&path).filter(Value::is_object).unwrap_or_else(|| {
            json!({
                "project": {"digest": project_digest(root), "name": project_name(root), "root": root.to_string_lossy()},
                "session_id": session, "messages": [],
            })
        });
        match ledger.get_mut("messages").and_then(Value::as_array_mut) {
            Some(existing) => existing
                .extend(spoken(exchanges).map(|(peer, content, stamp)| json!({"peer_id": peer, "content": content, "created_at": stamp}))),
            None => return Err(format!("invalid chat ledger: {}", path.display())),
        }
        ledger["updated_at"] = json!(now());
        atomic_write(&path, ledger.to_string().as_bytes()).map_err(|e| e.to_string())
    })?;
    Ok(spoken(exchanges).count())
}

fn obsidian_note(vault: &Path, root: &Path, session: &str) -> PathBuf {
    obsidian_dir(vault, root, "Chats").join(format!("{session}.md"))
}

fn record_obsidian(vault: &Path, root: &Path, session: &str, exchanges: &[Exchange]) -> Result<usize, String> {
    if config::obsidian_ai_chats(vault) { record_ai_chat(vault, root, session, exchanges) } else { record_callouts(vault, root, session, exchanges) }
}

/// What a session's note already holds, in either layout.
fn obsidian_messages(vault: &Path, root: &Path, session: &str) -> Vec<(String, String)> {
    if !config::obsidian_ai_chats(vault) {
        return callout_messages(&obsidian_note(vault, root, session));
    }
    ai_chat_note(vault, root, session)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| heading_messages(front_matter(&text).1))
        .unwrap_or_default()
}

// --- The vault's own AI-chat convention ----------------------------------------------------------
//
// `AI Chats/Chats/<platform>/<platform> <date> - <title> - <id>.md`, front matter `type: ai-chat`,
// messages as `### 01. User — <time>` sections under `## Conversation`, and the chat listed in
// `AI Chats/Sources/AI Source - <platform>.md` and `AI Chats/Projects/AI Project - <project>.md`.

fn ai_chats_dir(vault: &Path) -> PathBuf {
    vault.join(AI_CHATS).join("Chats").join(PLATFORM)
}

/// Matches a message heading; group 1 holds the backslashes of a content line that only looks
/// like one (written with one extra backslash, removed again on reading).
fn message_heading() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"^(\\*)### \d+\. (User|Assistant) — ").expect("valid regex"))
}

/// The note of a session, found by the id that ends its name.
fn ai_chat_note(vault: &Path, root: &Path, session: &str) -> Option<PathBuf> {
    let ending = format!(" - {}.md", &sha256_hex(&format!("{}/{session}", project_digest(root)))[..8]);
    std::fs::read_dir(ai_chats_dir(vault)).ok()?.flatten().map(|entry| entry.path()).find(|path| path.to_string_lossy().ends_with(&ending))
}

fn heading_messages(body: &str) -> Vec<(String, String)> {
    let conversation = body.split_once(CONVERSATION_HEADING).map_or("", |(_, conversation)| conversation);
    let mut messages: Vec<(String, String)> = Vec::new();
    for line in conversation.lines() {
        match message_heading().captures(line) {
            Some(found) if found[1].is_empty() => messages.push((found[2].to_lowercase(), String::new())),
            found => {
                if let Some((_, content)) = messages.last_mut() {
                    content.push_str(if found.is_some() { &line[1..] } else { line });
                    content.push('\n');
                }
            }
        }
    }
    for (_, content) in &mut messages {
        *content = content.trim().to_string();
    }
    messages.retain(|(_, content)| !content.is_empty() && !security::has_secret_content(content));
    messages
}

/// Add `line` to an index note once, creating the note from `header` when it does not exist.
fn list_in(path: &Path, header: String, line: &str) -> Result<(), String> {
    std::fs::create_dir_all(path.parent().expect("index dir")).map_err(|e| e.to_string())?;
    with_lock(path, || {
        let mut note = match std::fs::read_to_string(path) {
            Ok(existing) => existing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => header,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        if note.lines().any(|existing| existing == line) {
            return Ok(());
        }
        if !note.ends_with('\n') {
            note.push('\n');
        }
        note.push_str(line);
        note.push('\n');
        atomic_write(path, note.as_bytes()).map_err(|e| e.to_string())
    })
}

fn record_ai_chat(vault: &Path, root: &Path, session: &str, exchanges: &[Exchange]) -> Result<usize, String> {
    let Some((_, _, first)) = spoken(exchanges).next() else { return Ok(0) };
    let dir = ai_chats_dir(vault);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let (project, created) = (project_name(root), utc(first).0);
    let source_link = format!("[[AI Source - {PLATFORM}]]");
    let existing = ai_chat_note(vault, root, session);
    let path = existing.clone().unwrap_or_else(|| {
        dir.join(format!("{PLATFORM} {} - {session} - {}.md", &created[..10], &sha256_hex(&format!("{}/{session}", project_digest(root)))[..8]))
    });
    with_lock(&path, || {
        let note = match &existing {
            Some(path) => std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?,
            None => format!(
                "---\ntype: ai-chat\nplatform: \"{PLATFORM}\"\nsource: \"{PLATFORM}\"\nconversation_id: \"{}/{session}\"\ntitle: {}\nsession: {}\nproject: {}\nproject_root: {}\ncreated: \"{created}\"\nupdated: \"{created}\"\nmessage_count: 0\ntags: [ai-chat, context-broker]\n---\n\n# {session}\n\nSource: {source_link}\nProjects: [[AI Project - {project}]]\nMessages: 0{CONVERSATION_HEADING}",
                project_digest(root),
                yaml_quote(session),
                yaml_quote(session),
                yaml_quote(&project),
                yaml_quote(&root.to_string_lossy()),
            ),
        };
        let (head, conversation) = note.split_once(CONVERSATION_HEADING).ok_or_else(|| format!("{} has no Conversation section", path.display()))?;
        let mut count = conversation.lines().filter(|line| message_heading().captures(line).is_some_and(|found| found[1].is_empty())).count();
        let mut conversation = conversation.to_string();
        let mut updated = first;
        for (peer, content, stamp) in spoken(exchanges) {
            count += 1;
            updated = stamp;
            conversation.push_str(&format!("\n### {count:02}. {} — {}\n\n", if peer == "user" { "User" } else { "Assistant" }, utc(stamp).0));
            for line in content.trim().lines() {
                if message_heading().is_match(line) {
                    conversation.push('\\');
                }
                conversation.push_str(line);
                conversation.push('\n');
            }
        }
        let head: Vec<String> = head
            .lines()
            .map(|line| match line.split_once(' ').map(|(key, _)| key) {
                Some("updated:") => format!("updated: \"{}\"", utc(updated).0),
                Some("message_count:") => format!("message_count: {count}"),
                Some("Messages:") => format!("Messages: {count}"),
                _ => line.to_string(),
            })
            .collect();
        atomic_write(&path, format!("{}{CONVERSATION_HEADING}{conversation}", head.join("\n")).as_bytes()).map_err(|e| e.to_string())
    })?;
    if existing.is_none() {
        // A new chat is listed where the vault lists its chats: under its source and its project.
        let entry = format!("- {} — [[{}]]", &created[..10], path.file_stem().expect("note name").to_string_lossy());
        let base = vault.join(AI_CHATS);
        list_in(
            &base.join("Sources").join(format!("AI Source - {PLATFORM}.md")),
            format!("---\ntype: ai-chat-source\nplatform: \"{PLATFORM}\"\ntags: [ai-chat-source]\n---\n\n# AI Source - {PLATFORM}\n\nChats recorded by the Context Broker MCP server.\n\n## Chats\n\n"),
            &entry,
        )?;
        list_in(
            &base.join("Projects").join(format!("AI Project - {project}.md")),
            format!(
                "---\ntype: ai-project\nplatform: \"{PLATFORM}\"\nsource: \"{PLATFORM}\"\ntitle: {}\nproject_root: {}\ncreated: \"{created}\"\ntags: [ai-project]\n---\n\n# AI Project - {project}\n\nSource: {source_link}\n\n## Connected chats\n\n",
                yaml_quote(&project),
                yaml_quote(&root.to_string_lossy()),
            ),
            &entry,
        )?;
    }
    Ok(spoken(exchanges).count())
}

// --- The broker's own layout ---------------------------------------------------------------------

/// One note per session. Each message is a callout, so every content line is `>`-prefixed and no
/// message text can be mistaken for the start of another message.
fn record_callouts(vault: &Path, root: &Path, session: &str, exchanges: &[Exchange]) -> Result<usize, String> {
    let Some((_, _, first)) = spoken(exchanges).next() else { return Ok(0) };
    let path = obsidian_note(vault, root, session);
    std::fs::create_dir_all(path.parent().expect("chats dir")).map_err(|e| e.to_string())?;
    with_lock(&path, || {
        let mut note = match std::fs::read_to_string(&path) {
            Ok(existing) => existing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => format!(
                "---\ntype: context-broker-chat\nproject: {}\nroot: {}\nsession: {}\ncreated: {}\ntags: [context-broker]\n---\n# {session}\n",
                yaml_quote(&project_name(root)),
                yaml_quote(&root.to_string_lossy()),
                yaml_quote(session),
                utc(first).0,
            ),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        for (peer, content, stamp) in spoken(exchanges) {
            let kind = if peer == "user" { "quote" } else { "info" };
            note.push_str(&format!("\n> [!{kind}] {peer} · {}\n", utc(stamp).0));
            for line in content.lines() {
                note.push_str(if line.is_empty() { ">" } else { "> " });
                note.push_str(line);
                note.push('\n');
            }
        }
        atomic_write(&path, note.as_bytes()).map_err(|e| e.to_string())
    })?;
    Ok(spoken(exchanges).count())
}

/// One memory doc per exchange, in the layout of `graphify save-result`. A doc's name is fixed by
/// its time, position and text, so writing the same exchanges again adds nothing.
fn record_graphify(root: &Path, session: &str, exchanges: &[Exchange]) -> Result<usize, String> {
    let dir = graphify_memory(root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut written = 0;
    for (order, (stamp, user, assistant)) in exchanges.iter().enumerate() {
        let messages = [user, assistant].iter().filter(|content| !content.trim().is_empty()).count();
        let (iso, compact) = utc(*stamp);
        let slug: String = user.to_lowercase().chars().take(40).map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        // Stamp, then sub-second and position: file names sort in conversation order.
        let path = dir.join(format!("chat_{compact}_{:06}{order:03}_{}.md", (stamp.fract() * 1e6) as u32, slug.trim_matches('_')));
        if messages == 0 || path.exists() {
            continue;
        }
        let doc = format!(
            "---\ntype: \"chat\"\ndate: \"{iso}\"\nquestion: {}\ncontributor: \"context-broker\"\nsession: {}\n---\n\n# Q: {}{ANSWER_HEADING}{}\n",
            yaml_quote(user),
            yaml_quote(session),
            user.trim(),
            assistant.trim(),
        );
        atomic_write(&path, doc.as_bytes()).map_err(|e| e.to_string())?;
        written += messages;
    }
    Ok(written)
}

/// Regular `.ext` files directly inside `bases`, newest first, capped.
fn recent_files(bases: &[PathBuf], ext: &str, cap: usize) -> Vec<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = bases
        .iter()
        .filter_map(|base| std::fs::read_dir(base).ok())
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            (meta.is_file() && path.extension().is_some_and(|e| e == ext) && meta.len() <= MAX_HISTORY_FILE_BYTES)
                .then(|| (meta.modified().unwrap_or(UNIX_EPOCH), path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(cap).map(|(_, path)| path).collect()
}

fn json_messages(path: &Path) -> Vec<(String, String)> {
    read_json(path)
        .and_then(|ledger| ledger.get("messages").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| Some((m.get("peer_id")?.as_str()?.to_string(), m.get("content")?.as_str()?.to_string())))
        .collect()
}

/// Messages of an Obsidian session note. Notes can be edited by hand, so secrets are dropped here.
fn callout_messages(path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut messages: Vec<(String, String)> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let Some(quoted) = line.strip_prefix('>') else {
            inside = false;
            continue;
        };
        let quoted = quoted.strip_prefix(' ').unwrap_or(quoted);
        if inside {
            let (_, content) = messages.last_mut().expect("open callout");
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(quoted);
        } else if let Some((_, title)) = quoted.strip_prefix("[!").and_then(|rest| rest.split_once("] ")) {
            messages.push((title.split_whitespace().next().unwrap_or("note").to_string(), String::new()));
            inside = true;
        }
    }
    messages.retain(|(_, content)| !content.trim().is_empty() && !security::has_secret_content(content));
    messages
}

fn file_sessions(files: Vec<PathBuf>, only: Option<&str>, parse: fn(&Path) -> Vec<(String, String)>) -> Vec<Session> {
    files
        .iter()
        .filter_map(|path| {
            let name = path.file_stem()?.to_string_lossy().into_owned();
            only.is_none_or(|wanted| wanted == name).then(|| Session {
                source: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                messages: parse(path),
                name,
            })
        })
        .collect()
}

/// A memory doc: its file name and the messages it holds.
type MemoryDoc = (String, Vec<(String, String)>);

/// Memory docs grouped by session. Docs written by Graphify itself (`graphify save-result`) carry
/// no session and are read as the session `graphify`, so its saved answers count as history too.
fn graphify_sessions(root: &Path, only: Option<&str>) -> Vec<Session> {
    let mut grouped: Vec<(String, Vec<MemoryDoc>)> = Vec::new();
    for path in recent_files(&[graphify_memory(root)], "md", MAX_MEMORY_DOCS) {
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let (meta, body) = front_matter(&text);
        let Some(exchange) = body.trim_start().strip_prefix("# Q:").filter(|_| !file.starts_with("result_")) else {
            continue;
        };
        let name = meta.get("session").cloned().unwrap_or_else(|| "graphify".into());
        if only.is_some_and(|wanted| wanted != name) {
            continue;
        }
        let (question, answer) = exchange.split_once(ANSWER_HEADING).unwrap_or((exchange, ""));
        // Graphify's own docs end with bookkeeping sections that are not part of the answer.
        let answer = if meta.contains_key("session") {
            answer
        } else {
            ["\n\n## Outcome\n", "\n\n## Source Nodes\n"].iter().fold(answer, |rest, cut| rest.split_once(cut).map_or(rest, |(kept, _)| kept))
        };
        let messages: Vec<(String, String)> = [("user", question.trim()), ("assistant", answer.trim())]
            .into_iter()
            .filter(|(_, content)| !content.is_empty() && !security::has_secret_content(content))
            .map(|(peer, content)| (peer.to_string(), content.to_string()))
            .collect();
        match grouped.iter_mut().find(|(known, _)| *known == name) {
            Some((_, docs)) => docs.push((file, messages)),
            None => grouped.push((name, vec![(file, messages)])),
        }
    }
    grouped
        .into_iter()
        .map(|(name, mut docs)| {
            docs.sort_by(|a, b| a.0.cmp(&b.0));
            Session { source: name.clone(), name, messages: docs.into_iter().flat_map(|(_, messages)| messages).collect() }
        })
        .collect()
}

/// Stored sessions of the project, most recently written first.
fn sessions(root: &Path, only: Option<&str>) -> Result<Vec<Session>, String> {
    Ok(match config::store(root)? {
        Store::Json => {
            let bases = [chats_dir(root), root.join(IN_PROJECT_FOLDER).join("chats").join(project_digest(root))];
            file_sessions(recent_files(&bases, "json", MAX_HISTORY_FILES), only, json_messages)
        }
        Store::Obsidian(vault) if config::obsidian_ai_chats(&vault) => {
            // One folder holds every project's chats; a note says which project and session it is.
            let wanted = clip(&root.to_string_lossy(), 300);
            recent_files(&[ai_chats_dir(&vault)], "md", usize::MAX)
                .iter()
                .filter_map(|path| {
                    let text = std::fs::read_to_string(path).ok()?;
                    let (meta, body) = front_matter(&text);
                    let name = meta.get("session").filter(|_| meta.get("project_root") == Some(&wanted))?.clone();
                    only.is_none_or(|session| session == name).then(|| Session {
                        source: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        messages: heading_messages(body),
                        name,
                    })
                })
                .take(MAX_HISTORY_FILES)
                .collect()
        }
        Store::Obsidian(vault) => {
            file_sessions(recent_files(&[obsidian_dir(&vault, root, "Chats")], "md", MAX_HISTORY_FILES), only, callout_messages)
        }
        Store::Graphify => graphify_sessions(root, only),
    })
}

fn clip(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max_chars { format!("{}…", flat.chars().take(max_chars).collect::<String>().trim_end()) } else { flat }
}

/// The stored conversation, newest `limit` messages, as compact text.
pub fn load_session(root: &Path, session_id: &str, search_query: &str, limit: usize) -> Result<String, String> {
    let session = safe_id(session_id);
    let needle = search_query.to_lowercase();
    let messages: Vec<(String, String)> = sessions(root, Some(&session))?
        .into_iter()
        .flat_map(|stored| stored.messages)
        .filter(|(_, content)| needle.is_empty() || content.to_lowercase().contains(&needle))
        .collect();
    if messages.is_empty() {
        return Ok(format!("No stored messages for session '{session}'."));
    }
    let skip = messages.len().saturating_sub(limit.max(1));
    let mut out = format!("session {session} · {} of {} messages", messages.len() - skip, messages.len());
    for (peer, content) in &messages[skip..] {
        out.push_str(&format!("\n[{peer}] {content}"));
    }
    Ok(out)
}

/// Messages containing `search_query` across every session of the project.
pub fn cross_session(root: &Path, search_query: &str, top_k: usize) -> Result<String, String> {
    let needle = search_query.trim().to_lowercase();
    if needle.is_empty() {
        return Err("search_query is required".into());
    }
    let mut out = String::new();
    let mut found = 0;
    'sessions: for stored in sessions(root, None)? {
        for (peer, content) in stored.messages.iter().rev() {
            if content.to_lowercase().contains(&needle) {
                out.push_str(&format!("\n[{}/{peer}] {}", stored.name, clip(content, 600)));
                found += 1;
                if found >= top_k.clamp(1, 50) {
                    break 'sessions;
                }
            }
        }
    }
    Ok(if found == 0 { format!("No stored messages match '{search_query}'.") } else { format!("{found} match(es), newest sessions first{out}") })
}

fn history_terms(text: &str) -> HashSet<String> {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| Regex::new(r"\w+").expect("valid regex"));
    let stop: HashSet<&str> = HISTORY_STOPWORDS.split_whitespace().collect();
    rx.find_iter(&text.to_lowercase())
        .map(|m| m.as_str().to_string())
        .filter(|word| word.chars().count() > 2 && !stop.contains(word.as_str()))
        .collect()
}

/// What history lookups compare against: each stored message, joined with the short answer right
/// after it when both fit. Newest sessions first, without repeats or secret-bearing text.
fn exchanges(root: &Path) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for stored in sessions(root, None).unwrap_or_default() {
        let messages = &stored.messages;
        let mut index = 0;
        while let Some((_, content)) = messages.get(index) {
            index += 1;
            let mut text = content.clone();
            if let Some((peer, answer)) = messages.get(index)
                && peer.contains("assistant") && text.chars().count() + answer.chars().count() < MAX_EXCERPT_CHARS {
                    text = format!("{text}\n{answer}");
                    // The answer is part of this exchange; it is not a candidate of its own.
                    index += 1;
                }
            if text.chars().count() <= MAX_EXCERPT_CHARS && !security::has_secret_content(&text) && seen.insert(text.clone()) {
                found.push((stored.source.clone(), text));
            }
        }
    }
    found
}

/// Past exchanges that strongly overlap `query` (>= 2 shared terms and >= 60% of the query).
pub fn history_matches(root: &Path, query: &str) -> Vec<(String, String)> {
    let terms = history_terms(query);
    if terms.len() < 2 {
        return Vec::new();
    }
    let mut matches: Vec<(f64, String, String)> = exchanges(root)
        .into_iter()
        .filter_map(|(source, text)| {
            let overlap = terms.intersection(&history_terms(&text)).count();
            let score = overlap as f64 / terms.len() as f64;
            (overlap >= 2 && score >= 0.6).then_some((score, source, text))
        })
        .collect();
    matches.sort_by(|a, b| b.0.total_cmp(&a.0));
    matches.into_iter().take(3).map(|(_, source, text)| (source, text)).collect()
}

/// Turns texts into normalized embedding vectors (the search engine's model).
pub type Embed<'a> = &'a mut dyn FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>;

/// Embeddings of stored exchanges, kept on disk by content hash so each is computed once.
fn recall_cache(root: &Path) -> PathBuf {
    config::storage_dir().join("recall").join(format!("{}.bin", project_digest(root)))
}

/// File layout: the vector length (u32), then per exchange its SHA-256 and its vector (f32 each).
fn load_vectors(path: &Path) -> HashMap<[u8; 32], Vec<f32>> {
    let bytes = std::fs::read(path).unwrap_or_default();
    let Some((head, records)) = bytes.split_at_checked(4) else { return HashMap::new() };
    let dim = u32::from_le_bytes(head.try_into().expect("four bytes")) as usize;
    if dim == 0 || records.len() % (32 + dim * 4) != 0 {
        return HashMap::new();
    }
    records
        .chunks_exact(32 + dim * 4)
        .map(|record| {
            let (key, vector) = record.split_at(32);
            (key.try_into().expect("32 bytes"), vector.chunks_exact(4).map(|f| f32::from_le_bytes(f.try_into().expect("four bytes"))).collect())
        })
        .collect()
}

/// Past exchanges that mean the same as `query`, even when they share no words with it.
fn semantic_matches(root: &Path, query: &str, embed: Embed) -> Result<Vec<(String, String)>, String> {
    let candidates: Vec<(String, String)> = exchanges(root).into_iter().take(RECALL_CANDIDATES).collect();
    if candidates.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<[u8; 32]> = candidates
        .iter()
        .map(|(_, text)| {
            let mut key = [0u8; 32];
            key.copy_from_slice(&Sha256::digest(text.as_bytes()));
            key
        })
        .collect();
    let path = recall_cache(root);
    let mut cached = load_vectors(&path);
    let missing: Vec<usize> = (0..candidates.len()).filter(|&index| !cached.contains_key(&keys[index])).collect();
    // One model call: the exchanges not embedded yet, then the query.
    let mut inputs: Vec<String> = missing.iter().map(|&index| candidates[index].1.clone()).collect();
    inputs.push(query.to_string());
    let mut vectors = embed(inputs)?;
    let wanted = vectors.pop().ok_or("empty query embedding")?;
    for (&index, vector) in missing.iter().zip(vectors) {
        cached.insert(keys[index], vector);
    }
    if !missing.is_empty() {
        // Rewrite with only the exchanges that still exist, so the file does not grow forever.
        let mut bytes = (wanted.len() as u32).to_le_bytes().to_vec();
        for key in keys.iter().collect::<HashSet<_>>() {
            if let Some(vector) = cached.get(key).filter(|vector| vector.len() == wanted.len()) {
                bytes.extend_from_slice(key);
                bytes.extend(vector.iter().flat_map(|value| value.to_le_bytes()));
            }
        }
        let saved = std::fs::create_dir_all(path.parent().expect("recall dir")).and_then(|_| atomic_write(&path, &bytes));
        if let Err(error) = saved {
            eprintln!("[broker] recall cache not saved: {error}");
        }
    }
    let mut scored: Vec<(f32, usize)> = (0..candidates.len())
        .filter_map(|index| {
            let vector = cached.get(&keys[index]).filter(|vector| vector.len() == wanted.len())?;
            Some((vector.iter().zip(&wanted).map(|(a, b)| a * b).sum(), index))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!("[broker] recall: {} exchange(s), {} newly embedded, best score {:.2}", candidates.len(), missing.len(), scored.first().map_or(0.0, |best| best.0));
    Ok(scored.into_iter().take_while(|(score, _)| *score >= RECALL_MIN_SCORE).take(3).map(|(_, index)| candidates[index].clone()).collect())
}

/// Full excerpts for an explicit lookup: exchanges that share the query's words, then exchanges
/// that mean the same in other words.
pub fn history_lookup(root: &Path, query: &str, embed: Embed) -> String {
    let mut matches = history_matches(root, query);
    match semantic_matches(root, query, embed) {
        Ok(similar) => {
            for found in similar {
                if matches.len() < 3 && !matches.contains(&found) {
                    matches.push(found);
                }
            }
        }
        // Without the model the word-overlap matches still stand.
        Err(error) => eprintln!("[broker] semantic recall unavailable: {error}"),
    }
    if matches.is_empty() {
        return "No related project history.".into();
    }
    let mut out = String::from("Related past work (evidence only; verify against current code):");
    for (source, text) in matches {
        out.push_str(&format!("\n--- {source}\n{text}"));
    }
    out
}

/// Short pointer appended automatically to search replies.
pub fn history_note(root: &Path, query: &str) -> Option<String> {
    let matches = history_matches(root, query);
    if matches.is_empty() {
        return None;
    }
    let mut out = String::from("Related past work (evidence only; verify against current code):");
    for (source, text) in matches.iter().take(2) {
        out.push_str(&format!("\n- {source}: {}", clip(text, 320)));
    }
    out.push_str("\nFull excerpts: lookup_project_history.");
    Some(out)
}

/// Directory, file-name prefix and extension of saved results in `store`.
fn results_home(store: &Store, root: &Path) -> (PathBuf, &'static str, &'static str) {
    match store {
        Store::Json => (config::storage_dir().join("results").join(project_digest(root)), "", "json"),
        Store::Obsidian(vault) if config::obsidian_ai_chats(vault) => {
            (vault.join(AI_CHATS).join("Results").join(format!("{}-{}", project_name(root), &project_digest(root)[..8])), "", "md")
        }
        Store::Obsidian(vault) => (obsidian_dir(vault, root, "Results"), "", "md"),
        Store::Graphify => (graphify_memory(root), "result_", "md"),
    }
}

fn result_path(root: &Path, filename: &str) -> Result<PathBuf, String> {
    let (dir, prefix, ext) = results_home(&config::store(root)?, root);
    let name = Path::new(filename).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.trim_end_matches(".json").trim_end_matches(".md");
    if stem.is_empty() || stem.starts_with('.') {
        return Err("filename is required".into());
    }
    Ok(dir.join(format!("{prefix}{stem}.{ext}")))
}

/// A saved result as a Markdown doc (the Obsidian and Graphify layout).
fn result_doc(query: &str, text: &str, stamp: f64) -> String {
    format!(
        "---\ntype: \"search\"\ndate: \"{}\"\nquestion: {}\ncontributor: \"context-broker\"\n---\n\n# Q: {}{ANSWER_HEADING}{text}\n",
        utc(stamp).0,
        yaml_quote(query),
        clip(query, 300),
    )
}

pub fn save_results(root: &Path, filename: &str, query: &str, text: &str) -> Result<String, String> {
    let path = result_path(root, filename)?;
    std::fs::create_dir_all(path.parent().expect("results dir")).map_err(|e| e.to_string())?;
    let payload = if path.extension().is_some_and(|ext| ext == "json") {
        json!({"project_root": root.to_string_lossy(), "query": query, "saved_at": now(), "results": text}).to_string()
    } else {
        result_doc(query, text, now())
    };
    atomic_write(&path, payload.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("Saved to {}", path.display()))
}

pub fn list_results(root: &Path) -> Result<String, String> {
    let (dir, prefix, ext) = results_home(&config::store(root)?, root);
    let suffix = format!(".{ext}");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(&suffix))
                .filter_map(|name| name.strip_prefix(prefix).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    Ok(if names.is_empty() { "No saved results.".into() } else { names.join("\n") })
}

pub fn load_results(root: &Path, filename: &str) -> Result<String, String> {
    let path = result_path(root, filename)?;
    let missing = || format!("saved result not found: {filename}");
    if path.extension().is_some_and(|ext| ext == "json") {
        let payload = read_json(&path).ok_or_else(missing)?;
        return Ok(format!("query: {}\n{}", payload["query"].as_str().unwrap_or(""), payload["results"].as_str().unwrap_or("")));
    }
    let text = std::fs::read_to_string(&path).map_err(|_| missing())?;
    let (meta, body) = front_matter(&text);
    let results = body.split_once(ANSWER_HEADING).map_or(body, |(_, results)| results);
    Ok(format!("query: {}\n{}", meta.get("question").map_or("", String::as_str), results.trim_end()))
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "json")).collect())
        .unwrap_or_default();
    files.sort();
    files
}

/// Message text as it reads back from a note, for telling what a note already holds.
fn canonical(content: &str) -> String {
    content.trim().lines().collect::<Vec<_>>().join("\n")
}

/// Copy what the JSON store holds (chat sessions and saved results) into Obsidian or Graphify.
/// Repeating it adds nothing: messages and results already there are skipped.
pub fn export(root: &Path, target: &str, all_projects: bool) -> Result<String, String> {
    let store = config::resolve_store(if target.trim().is_empty() { "auto" } else { target }, root)?;
    let place = match &store {
        Store::Json => return Err("no Obsidian vault or Graphify found on this machine; set CONTEXT_BROKER_OBSIDIAN_VAULT, or build a graph with Graphify, then retry".into()),
        Store::Obsidian(vault) => format!("Obsidian vault {}", vault.display()),
        Store::Graphify => format!("Graphify memory ({GRAPHIFY_OUT}/memory)"),
    };
    let storage = config::storage_dir();
    // The project a stored folder belongs to, read from the first file that names it.
    let owner = |digest: &str| -> Option<PathBuf> {
        ["chats", "results"].iter().flat_map(|kind| json_files(&storage.join(kind).join(digest))).find_map(|file| {
            let stored = read_json(&file)?;
            let path = stored.pointer("/project/root").or_else(|| stored.get("project_root"))?.as_str()?;
            Some(PathBuf::from(path)).filter(|path| path.is_dir() && project_digest(path) == digest)
        })
    };
    let (mut projects, mut missing) = (vec![root.to_path_buf()], 0);
    if all_projects {
        let digests: HashSet<String> = ["chats", "results"]
            .iter()
            .filter_map(|kind| std::fs::read_dir(storage.join(kind)).ok())
            .flat_map(|entries| entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect();
        for digest in digests.iter().filter(|digest| **digest != project_digest(root)) {
            match owner(digest) {
                Some(project) => projects.push(project),
                None => missing += 1,
            }
        }
    }
    let (mut sessions, mut messages, mut results) = (0, 0, 0);
    for project in &projects {
        let digest = project_digest(project);
        let ledgers = [chats_dir(project), project.join(IN_PROJECT_FOLDER).join("chats").join(&digest)];
        for file in ledgers.iter().flat_map(|dir| json_files(dir)) {
            let Some(ledger) = read_json(&file) else { continue };
            let session = file.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
            let fallback = ledger.get("updated_at").and_then(Value::as_f64).unwrap_or_else(now);
            let stored: Vec<(bool, &str, f64)> = ledger
                .get("messages")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(|m| {
                    let from_assistant = m.get("peer_id")?.as_str()?.contains("assistant");
                    Some((from_assistant, m.get("content")?.as_str()?, m.get("created_at").and_then(Value::as_f64).unwrap_or(fallback)))
                }).collect())
                .unwrap_or_default();
            // A question and the answer right after it form one exchange.
            let mut exchanges: Vec<Exchange> = Vec::new();
            let mut index = 0;
            while let Some(&(from_assistant, content, stamp)) = stored.get(index) {
                index += 1;
                match stored.get(index) {
                    _ if from_assistant => exchanges.push((stamp, "", content)),
                    Some(&(true, answer, _)) => {
                        exchanges.push((stamp, content, answer));
                        index += 1;
                    }
                    _ => exchanges.push((stamp, content, "")),
                }
            }
            if let Store::Obsidian(vault) = &store {
                // Blank out what the note already holds, so only new messages are appended.
                let mut held: HashMap<(String, String), usize> = HashMap::new();
                for (peer, content) in obsidian_messages(vault, project, &session) {
                    *held.entry((peer, canonical(&content))).or_default() += 1;
                }
                let mut fresh = |peer: &str, content| match held.get_mut(&(peer.to_string(), canonical(content))) {
                    Some(count) if *count > 0 => {
                        *count -= 1;
                        ""
                    }
                    _ => content,
                };
                for (_, user, assistant) in &mut exchanges {
                    (*user, *assistant) = (fresh("user", user), fresh("assistant", assistant));
                }
            }
            sessions += 1;
            messages += write(&store, project, &session, &exchanges)?;
        }
        let (dir, prefix, ext) = results_home(&store, project);
        for file in json_files(&storage.join("results").join(&digest)) {
            let (Some(saved), Some(stem)) = (read_json(&file), file.file_stem()) else { continue };
            let path = dir.join(format!("{prefix}{}.{ext}", stem.to_string_lossy()));
            if path.exists() {
                continue;
            }
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let doc = result_doc(saved["query"].as_str().unwrap_or(""), saved["results"].as_str().unwrap_or(""), saved["saved_at"].as_f64().unwrap_or_else(now));
            atomic_write(&path, doc.as_bytes()).map_err(|e| e.to_string())?;
            results += 1;
        }
    }
    let mut reply = format!("Exported to {place}: {messages} new message(s) from {sessions} session(s), {results} new saved result(s), {} project(s).", projects.len());
    if missing > 0 {
        reply.push_str(&format!(" {missing} stored project(s) skipped: folder no longer exists."));
    }
    Ok(reply)
}
