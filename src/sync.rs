//! File sync for remote copies of the project folder (an Obsidian vault and the plugin that
//! mirrors it): a manifest of files with content hashes, and read, write and delete of single
//! files guarded by the hash the caller last saw, so two devices cannot silently overwrite each
//! other. Hidden files and folders (`.obsidian`, `.git`, ...) are never listed or touched.

use crate::index::hex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

pub const MAX_FILE_BYTES: u64 = 50_000_000;

pub enum Reply {
    Json(u16, Value),
    File(Vec<u8>, String),
}

pub struct Sync {
    root: PathBuf,
    /// Content hashes by path, valid while the file's modification time and size stay the same.
    hashes: Mutex<HashMap<String, (u64, u64, String)>>,
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The broker's own lock and temporary files are not content.
fn is_scratch(name: &str) -> bool {
    name.ends_with(".lock") || name.rsplit_once(".tmp").is_some_and(|(_, pid)| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

fn stamp(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map_or(0, |age| age.as_millis() as u64)
}

impl Sync {
    pub fn new(root: PathBuf) -> Self {
        Self { root, hashes: Mutex::new(HashMap::new()) }
    }

    /// The file a relative path names, or `None` for a path that is hidden, malformed or scratch.
    fn locate(&self, relative: &str) -> Option<PathBuf> {
        let parts: Vec<&str> = relative.split('/').collect();
        let fine = |part: &&str| !part.is_empty() && !part.starts_with('.') && !part.contains(['\\', '\0']) && *part != "..";
        (parts.iter().all(fine) && !is_scratch(parts.last()?)).then(|| parts.iter().fold(self.root.clone(), |path, part| path.join(part)))
    }

    /// True when the nearest existing folder above `path` really lies inside the root, so a
    /// symlinked folder cannot lead a read, write or delete out of the project.
    fn contained(&self, path: &Path) -> bool {
        path.ancestors().skip(1).find(|dir| std::fs::symlink_metadata(dir).is_ok()).and_then(|dir| dir.canonicalize().ok()).is_some_and(|real| real.starts_with(&self.root))
    }

    /// Hash of a regular file within the size limit; `None` when absent, a symlink or too large.
    fn hash_of(&self, relative: &str, path: &Path) -> Option<(String, u64, u64)> {
        let meta = std::fs::symlink_metadata(path).ok().filter(|meta| meta.is_file() && meta.len() <= MAX_FILE_BYTES)?;
        let (modified, size) = (stamp(&meta), meta.len());
        let mut hashes = self.hashes.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((seen, length, hash)) = hashes.get(relative).filter(|(seen, length, _)| *seen == modified && *length == size) {
            return Some((hash.clone(), *seen, *length));
        }
        let hash = sha256(&std::fs::read(path).ok()?);
        hashes.insert(relative.to_string(), (modified, size, hash.clone()));
        Some((hash, modified, size))
    }

    pub fn manifest(&self) -> Reply {
        let mut files = Vec::new();
        let mut pending = vec![(self.root.clone(), String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(kind) = entry.file_type() else { continue };
                if name.starts_with('.') || kind.is_symlink() || is_scratch(&name) {
                    continue;
                }
                let relative = format!("{prefix}{name}");
                if kind.is_dir() {
                    pending.push((entry.path(), format!("{relative}/")));
                } else if let Some((hash, modified, size)) = self.hash_of(&relative, &entry.path()) {
                    files.push(json!({"path": relative, "size": size, "mtime": modified, "hash": hash}));
                }
            }
        }
        files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Reply::Json(200, json!({"files": files}))
    }

    pub fn read(&self, relative: &str) -> Reply {
        let Some(path) = self.locate(relative).filter(|path| self.contained(path)) else { return Reply::Json(400, json!({"error": "bad path"})) };
        match self.hash_of(relative, &path).and_then(|(hash, ..)| Some((std::fs::read(&path).ok()?, hash))) {
            Some((bytes, hash)) => Reply::File(bytes, hash),
            None => Reply::Json(404, json!({"error": "not found"})),
        }
    }

    /// Write `body` if the server still has what the caller last saw (`base`, empty for "no file").
    pub fn write(&self, relative: &str, base: &str, body: &[u8]) -> Reply {
        let Some(path) = self.locate(relative).filter(|path| self.contained(path)) else { return Reply::Json(400, json!({"error": "bad path"})) };
        let current = self.hash_of(relative, &path).map(|(hash, ..)| hash).unwrap_or_default();
        let incoming = sha256(body);
        if current != base && current != incoming {
            return Reply::Json(409, json!({"hash": current}));
        }
        if current != incoming {
            // Folders are created as needed; the result must still be inside the project.
            let parent = path.parent().expect("file has a parent");
            let inside = std::fs::create_dir_all(parent).is_ok() && parent.canonicalize().is_ok_and(|real| real.starts_with(&self.root));
            if !inside || std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
                return Reply::Json(400, json!({"error": "bad path"}));
            }
            if let Err(error) = crate::memory::atomic_write(&path, body) {
                return Reply::Json(500, json!({"error": error.to_string()}));
            }
        }
        let modified = std::fs::metadata(&path).map(|meta| stamp(&meta)).unwrap_or(0);
        Reply::Json(200, json!({"hash": incoming, "mtime": modified}))
    }

    pub fn delete(&self, relative: &str, base: &str) -> Reply {
        let Some(path) = self.locate(relative).filter(|path| self.contained(path)) else { return Reply::Json(400, json!({"error": "bad path"})) };
        match self.hash_of(relative, &path) {
            None => Reply::Json(200, json!({"deleted": false})),
            Some((current, ..)) if current != base => Reply::Json(409, json!({"hash": current})),
            Some(_) => match std::fs::remove_file(&path) {
                Ok(()) => Reply::Json(200, json!({"deleted": true})),
                Err(error) => Reply::Json(500, json!({"error": error.to_string()})),
            },
        }
    }
}

/// Decode `%XX` escapes and `+` in a query-string value.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%').then(|| text.get(index + 1..index + 3).and_then(|pair| u8::from_str_radix(pair, 16).ok())).flatten();
        match escaped {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(if bytes[index] == b'+' { b' ' } else { bytes[index] });
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
