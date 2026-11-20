//! Local JSON persistence: chat ledgers, history lookup, saved search results.
//!
//! Ledger layout and schema match the Python broker
//! (`<storage>/chats/<project_digest>/<session>.json`), so existing history stays readable.

use crate::config::{self, IN_PROJECT_FOLDER};
use crate::index::hex;
use crate::project::project_name;
use crate::security;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_HISTORY_FILES: usize = 128;
const MAX_HISTORY_FILE_BYTES: u64 = 1_000_000;
const MAX_EXCERPT_CHARS: usize = 2_000;
const HISTORY_STOPWORDS: &str = "a an the is are was were be been to of for in on at with and or it this that \
my our your we you i me can could should would please how what why when where \
do does did has have had make sure fix issue problem help again";

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

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Append `(user, assistant)` turns to one session; prior turns are never overwritten.
pub fn record(root: &Path, session_id: &str, turns: &[(String, String)]) -> Result<String, String> {
    let session = safe_id(session_id);
    let stamp = now();
    let mut messages = Vec::new();
    for (user, assistant) in turns {
        for (peer, content) in [("user", user), ("assistant", assistant)] {
            if content.trim().is_empty() {
                continue;
            }
            if security::has_secret_content(content) {
                return Err("message contains potential secrets; nothing was saved".into());
            }
            messages.push(json!({"peer_id": peer, "content": content, "created_at": stamp}));
        }
    }
    if messages.is_empty() {
        return Err("nothing to record: every message is empty".into());
    }
    let path = chats_dir(root).join(format!("{session}.json"));
    std::fs::create_dir_all(path.parent().expect("chats dir")).map_err(|e| e.to_string())?;
    let saved = messages.len();
    with_lock(&path, || {
        let mut ledger = read_json(&path).filter(Value::is_object).unwrap_or_else(|| {
            json!({
                "project": {"digest": project_digest(root), "name": project_name(root), "root": root.to_string_lossy()},
                "session_id": session, "messages": [],
            })
        });
        match ledger.get_mut("messages").and_then(Value::as_array_mut) {
            Some(existing) => existing.extend(messages),
            None => return Err(format!("invalid chat ledger: {}", path.display())),
        }
        ledger["updated_at"] = json!(stamp);
        atomic_write(&path, ledger.to_string().as_bytes()).map_err(|e| e.to_string())
    })?;
    Ok(format!("Saved {saved} message(s) to session '{session}'."))
}

fn session_files(root: &Path) -> Vec<PathBuf> {
    let bases = [chats_dir(root), root.join(IN_PROJECT_FOLDER).join("chats").join(project_digest(root))];
    let mut files: Vec<(SystemTime, PathBuf)> = bases
        .iter()
        .filter_map(|base| std::fs::read_dir(base).ok())
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            (meta.is_file() && path.extension().is_some_and(|e| e == "json") && meta.len() <= MAX_HISTORY_FILE_BYTES)
                .then(|| (meta.modified().unwrap_or(UNIX_EPOCH), path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(MAX_HISTORY_FILES).map(|(_, path)| path).collect()
}

fn messages_of(path: &Path) -> Vec<(String, String)> {
    read_json(path)
        .and_then(|ledger| ledger.get("messages").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| Some((m.get("peer_id")?.as_str()?.to_string(), m.get("content")?.as_str()?.to_string())))
        .collect()
}

fn clip(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max_chars { format!("{}…", flat.chars().take(max_chars).collect::<String>().trim_end()) } else { flat }
}

/// The stored conversation, newest `limit` messages, as compact text.
pub fn load_session(root: &Path, session_id: &str, search_query: &str, limit: usize) -> Result<String, String> {
    let session = safe_id(session_id);
    let needle = search_query.to_lowercase();
    let messages: Vec<(String, String)> = session_files(root)
        .iter()
        .filter(|path| path.file_stem().is_some_and(|stem| stem.to_string_lossy() == session))
        .flat_map(|path| messages_of(path))
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
    'files: for path in session_files(root) {
        let session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        for (peer, content) in messages_of(&path).iter().rev() {
            if content.to_lowercase().contains(&needle) {
                out.push_str(&format!("\n[{session}/{peer}] {}", clip(content, 600)));
                found += 1;
                if found >= top_k.clamp(1, 50) {
                    break 'files;
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

/// Past exchanges that strongly overlap `query` (>= 2 shared terms and >= 60% of the query).
pub fn history_matches(root: &Path, query: &str) -> Vec<(String, String)> {
    let terms = history_terms(query);
    if terms.len() < 2 {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut matches: Vec<(f64, String, String)> = Vec::new();
    for path in session_files(root) {
        let source = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let messages = messages_of(&path);
        for (i, (_, content)) in messages.iter().enumerate() {
            // Keep a short adjacent answer with its question when both fit.
            let mut text = content.clone();
            if let Some((peer, answer)) = messages.get(i + 1)
                && peer.contains("assistant") && text.chars().count() + answer.chars().count() < MAX_EXCERPT_CHARS {
                    text = format!("{text}\n{answer}");
                }
            if text.chars().count() > MAX_EXCERPT_CHARS || security::has_secret_content(&text) {
                continue;
            }
            let overlap = terms.intersection(&history_terms(&text)).count();
            let score = overlap as f64 / terms.len() as f64;
            if overlap >= 2 && score >= 0.6 && seen.insert(text.clone()) {
                matches.push((score, source.clone(), text));
            }
        }
    }
    matches.sort_by(|a, b| b.0.total_cmp(&a.0));
    matches.into_iter().take(3).map(|(_, source, text)| (source, text)).collect()
}

/// Full excerpts for an explicit lookup.
pub fn history_lookup(root: &Path, query: &str) -> String {
    let matches = history_matches(root, query);
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

fn results_dir(root: &Path) -> PathBuf {
    config::storage_dir().join("results").join(project_digest(root))
}

fn result_path(root: &Path, filename: &str) -> Result<PathBuf, String> {
    let name = Path::new(filename).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.trim_end_matches(".json");
    if stem.is_empty() || stem.starts_with('.') {
        return Err("filename is required".into());
    }
    Ok(results_dir(root).join(format!("{stem}.json")))
}

pub fn save_results(root: &Path, filename: &str, query: &str, text: &str) -> Result<String, String> {
    let path = result_path(root, filename)?;
    std::fs::create_dir_all(path.parent().expect("results dir")).map_err(|e| e.to_string())?;
    let payload = json!({"project_root": root.to_string_lossy(), "query": query, "saved_at": now(), "results": text});
    atomic_write(&path, payload.to_string().as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("Saved to {}", path.display()))
}

pub fn list_results(root: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(results_dir(root))
        .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".json")).collect())
        .unwrap_or_default();
    names.sort();
    if names.is_empty() { "No saved results.".into() } else { names.join("\n") }
}

pub fn load_results(root: &Path, filename: &str) -> Result<String, String> {
    let path = result_path(root, filename)?;
    let payload = read_json(&path).ok_or_else(|| format!("saved result not found: {filename}"))?;
    Ok(format!(
        "query: {}\n{}",
        payload["query"].as_str().unwrap_or(""),
        payload["results"].as_str().unwrap_or("")
    ))
}
