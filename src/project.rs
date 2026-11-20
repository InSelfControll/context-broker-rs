//! Project root resolution and source file collection.

use crate::config::{self, IGNORE_DIRS, PROJECT_MARKERS, SUPPORTED_EXTENSIONS};
use crate::security;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Explicit argument, then `CONTEXT_BROKER_PROJECT_ROOT`, then marker detection, then CWD.
pub fn resolve_root(explicit: &str) -> Result<PathBuf, String> {
    let candidate = if !explicit.trim().is_empty() {
        PathBuf::from(explicit.trim())
    } else if let Some(env_root) = std::env::var("CONTEXT_BROKER_PROJECT_ROOT").ok().filter(|v| !v.is_empty()) {
        PathBuf::from(env_root)
    } else {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        detect_root(&cwd).unwrap_or(cwd)
    };
    let root = candidate.canonicalize().map_err(|e| format!("project root {}: {e}", candidate.display()))?;
    if !root.is_dir() {
        return Err(format!("project root is not a directory: {}", root.display()));
    }
    Ok(root)
}

/// Walk upwards and pick the ancestor with the strongest project markers.
fn detect_root(start: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for dir in start.ancestors().take(12) {
        let score: u32 = PROJECT_MARKERS.iter().filter(|(name, _)| dir.join(name).exists()).map(|(_, s)| s).sum();
        if score > 0 && best.as_ref().is_none_or(|(top, _)| score > *top) {
            best = Some((score, dir.to_path_buf()));
        }
    }
    best.map(|(_, dir)| dir)
}

pub fn project_name(root: &Path) -> String {
    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "unknown".into())
}

pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Indexable source files, sorted. Honors `.gitignore`/`.ignore`, never follows symlinks, and
/// always drops ignored directories, unsupported extensions, oversized and secret-named files.
pub fn collect_files(root: &Path) -> Vec<PathBuf> {
    let max_bytes = config::max_file_bytes();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(entry.file_type().is_some_and(|t| t.is_dir())
                && (IGNORE_DIRS.contains(&name.as_ref()) || name.ends_with(".egg-info")))
        })
        .build();
    let mut files: Vec<PathBuf> = walker
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter(|entry| {
            let path = entry.path();
            let supported = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
            supported
                && !security::is_secret_path(&relative(root, path))
                && entry.metadata().is_ok_and(|m| max_bytes == 0 || m.len() <= max_bytes)
        })
        .map(|entry| entry.into_path())
        .collect();
    files.sort();
    files
}

/// Read at most `max_chars` characters; `None` when unreadable or blocked as secret-bearing.
pub fn read_safe(root: &Path, path: &Path, max_chars: usize) -> Option<String> {
    if security::is_secret_path(&relative(root, path)) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let content: String = text.chars().take(max_chars).collect();
    if security::has_secret_content(&content) {
        return None;
    }
    Some(content)
}
