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

/// `NAME = "literal"` with a secret-like name, in the forms code and config files use: spaces
/// around the operator, a declaration keyword, a type annotation (`const API_KEY: string = "..."`,
/// `SECRET_KEY = '...'`, `password = "..."`). Only a quoted literal counts, so `token = make()`
/// stays readable; group 1 is the value.
fn quoted_assignment() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        let names = SECRET_KEY_SIGNATURES.join("|");
        Regex::new(&format!(
            r#"(?i)^\s*(?:(?:export|const|let|var|val|static|final|public|private|protected|readonly|pub|my|our|local|set)\s+)*\$?["']?[\w.\-]*(?:{names})[\w.\-]*["']?(?:\s*:\s*[\w<>\[\]?|.&']+)?\s*(?::=|=>|=|:)\s*[bru]?["']([^"'\s]{{8,}})["']"#
        ))
        .expect("valid secret regex")
    })
}

/// A literal that looks like a credential rather than a word: it has a digit, or is long.
fn credential_like(value: &str) -> bool {
    value.len() >= 20 || value.chars().any(|c| c.is_ascii_digit())
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

/// True when any line of `content` carries a secret signature. Comment lines are still checked
/// for real key material (a commented-out token is still a token); only the name-based
/// assignment checks skip them, so prose like `# set API_KEY=...` in docs stays readable.
pub fn has_secret_content(content: &str) -> bool {
    content.lines().any(|line| {
        let stripped = line.trim();
        if stripped.is_empty() {
            return false;
        }
        if SECRET_VALUE_SIGNATURES.iter().any(|sig| stripped.contains(sig)) || token_formats().is_match(line) {
            return true;
        }
        let comment = stripped.starts_with('#') || stripped.starts_with("//");
        !comment
            && (key_assignment().is_match(line)
                || quoted_assignment().captures(line).is_some_and(|found| credential_like(&found[1])))
    })
}

#[cfg(test)]
mod tests {
    use super::has_secret_content;

    #[test]
    fn detects_common_credential_forms() {
        // Every credential-shaped value is built at run time, so the source holds no literal that
        // a secret scanner would rightly flag.
        let value = "q7".repeat(6);
        let token = format!("ghp_{}", "a1".repeat(18));
        let cases = [
            format!("SECRET_KEY = 'django-insecure-{value}'"),
            format!("password = \"{value}\""),
            format!("const API_KEY = \"{value}\";"),
            format!("export const authToken: string = '{value}';"),
            format!("    db_password: \"{value}\""),
            format!("# GITHUB_TOKEN={token}"),
            format!("// fallback: {token}"),
            format!("API_KEY={value}"),
        ];
        for secret in &cases {
            assert!(has_secret_content(secret), "missed: {secret}");
        }
    }

    #[test]
    fn leaves_ordinary_code_readable() {
        for line in [
            "token = make_token()",
            "let token = parse(input);",
            "# set API_KEY=... in your shell",
            "// password = \"<your password>\" is only an example",
            "const TOKEN_KIND = \"identifier\";",
            "def login_user(name, password_hash):",
        ] {
            assert!(!has_secret_content(line), "blocked: {line}");
        }
    }
}
