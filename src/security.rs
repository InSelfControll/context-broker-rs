//! Secret protection: filename patterns plus content signatures, applied to every read.

use crate::config::{SECRET_FILE_PATTERNS, SECRET_KEY_SIGNATURES, SECRET_TOKEN_PATTERNS, SECRET_VALUE_SIGNATURES};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use std::sync::OnceLock;

fn file_patterns() -> &'static GlobSet {
    static SET: OnceLock<GlobSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut builder = GlobSetBuilder::new();
        for pattern in SECRET_FILE_PATTERNS {
            builder.add(Glob::new(pattern).expect("valid secret pattern"));
        }
        builder.build().expect("valid secret pattern set")
    })
}

/// `KEY=value` / `key: value` at the start of a line (optional `export` and quotes), so prose
/// and ordinary code such as `token = make_token()` are not blocked.
fn key_assignment() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        let names = SECRET_KEY_SIGNATURES.join("|");
        Regex::new(&format!(r#"(?i)^\s*(?:export\s+)?["']?[\w.\-]*(?:{names})[\w.\-]*["']?[:=]"#))
            .expect("valid secret regex")
    })
}

/// Well-known credential formats (cloud keys, personal access tokens, API keys).
fn token_formats() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(&SECRET_TOKEN_PATTERNS.join("|")).expect("valid token regex"))
}

/// True when the file name or relative path matches a hard-coded secret pattern.
pub fn is_secret_path(rel_path: &str) -> bool {
    let rel = rel_path.replace('\\', "/");
    let base = rel.rsplit('/').next().unwrap_or(&rel);
    file_patterns().is_match(base) || file_patterns().is_match(&rel)
}

/// True when any line of `content` carries a secret signature.
pub fn has_secret_content(content: &str) -> bool {
    content.lines().any(|line| {
        let stripped = line.trim();
        !stripped.is_empty()
            && !stripped.starts_with('#')
            && (SECRET_VALUE_SIGNATURES.iter().any(|sig| stripped.contains(sig))
                || key_assignment().is_match(line)
                || token_formats().is_match(line))
    })
}
