//! Exact literal / regex search over the same safe file set as the semantic index.

use crate::config;
use crate::project::{self, relative};
use globset::Glob;
use regex::RegexBuilder;
use std::path::Path;

const MAX_FILES: usize = 20;
const MAX_MATCHES_PER_FILE: usize = 10;
const MAX_LINE_CHARS: usize = 200;

pub fn search(root: &Path, pattern: &str, case_sensitive: bool, use_regex: bool, file_glob: &str) -> Result<String, String> {
    if pattern.is_empty() || pattern.len() > 500 {
        return Err("pattern must contain 1-500 characters".into());
    }
    let source = if use_regex { pattern.to_string() } else { regex::escape(pattern) };
    // The regex crate guarantees linear-time matching, so no timeout is needed.
    let matcher = RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| format!("invalid pattern: {e}"))?;
    let glob = match file_glob.trim() {
        "" => None,
        glob => Some(Glob::new(glob).map_err(|e| format!("invalid file_glob: {e}"))?.compile_matcher()),
    };

    let files = project::collect_files(root);
    let (mut body, mut total, mut with_matches, mut searched, mut truncated) = (String::new(), 0, 0, 0, false);
    for path in &files {
        let rel = relative(root, path);
        if glob.as_ref().is_some_and(|g| !g.is_match(&rel) && !g.is_match(rel.rsplit('/').next().unwrap_or(&rel))) {
            continue;
        }
        let Some(content) = project::read_safe(root, path, config::READ_MAX_CHARS * 5) else { continue };
        searched += 1;
        let hits: Vec<(usize, &str)> = content.lines().enumerate().filter(|(_, line)| matcher.is_match(line)).collect();
        if hits.is_empty() {
            continue;
        }
        total += hits.len();
        with_matches += 1;
        if with_matches > MAX_FILES {
            truncated = true;
            continue;
        }
        body.push_str(&format!("\n### {rel}"));
        for (number, line) in hits.iter().take(MAX_MATCHES_PER_FILE) {
            let shown: String = line.trim().chars().take(MAX_LINE_CHARS).collect();
            body.push_str(&format!("\nL{}: {shown}", number + 1));
        }
        truncated |= hits.len() > MAX_MATCHES_PER_FILE;
    }
    if total == 0 {
        return Ok(format!("No matches for '{pattern}' in {searched} files."));
    }
    let mut out = format!("{total} matches in {with_matches}/{searched} files · root: {}{body}", root.display());
    if truncated {
        out.push_str("\nResults truncated; narrow the pattern or pass file_glob.");
    }
    Ok(out)
}
