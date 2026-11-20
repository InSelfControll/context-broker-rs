//! Query-focused snippets: the lines that best match the query, within a token budget.

use regex::Regex;
use std::collections::BTreeSet;
use std::sync::OnceLock;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "this", "that", "where", "what", "when", "into", "only",
    "show", "find", "code", "file", "files", "tool", "about", "how", "does",
];

/// Cheap token estimate: code averages about 3.7 characters per token, plus the newline.
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() * 10).div_ceil(37) + 1
}

/// Lower-cased, de-duplicated query terms worth matching.
pub fn query_terms(query: &str) -> Vec<String> {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| Regex::new(r"[a-zA-Z0-9_./-]+").expect("valid regex"));
    let mut terms: Vec<String> = Vec::new();
    for found in rx.find_iter(&query.to_lowercase()) {
        let term = found.as_str();
        if term.len() >= 3 && !STOPWORDS.contains(&term) && !terms.iter().any(|t| t == term) {
            terms.push(term.to_string());
        }
    }
    terms.truncate(16);
    terms
}

fn is_definition(line: &str) -> bool {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        Regex::new(r"^\s*(?:export\s+|pub(?:\([a-z]+\))?\s+|async\s+|static\s+)*(?:def|class|function|func|fn|interface|struct|enum|trait|impl|type|const)\b")
            .expect("valid regex")
    })
    .is_match(line)
}

/// Returns `(snippet, truncated)`. Runs of lines are prefixed with `@L<line>` so the caller can
/// read more precisely; a file with no query term falls back to its head.
pub fn focused(content: &str, terms: &[String], token_limit: usize, context: usize) -> (String, bool) {
    let lines: Vec<&str> = content.lines().collect();
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    // A term that is rare in this file outweighs one that is on every line.
    let weights: Vec<(&String, f64)> = terms
        .iter()
        .filter_map(|term| {
            let frequency = lowered.iter().filter(|l| l.contains(term.as_str())).count();
            (frequency > 0).then(|| (term, 1.0 / frequency as f64))
        })
        .collect();
    let mut ranked: Vec<(f64, usize)> = lowered
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let mut score: f64 = weights.iter().filter(|(t, _)| line.contains(t.as_str())).map(|(_, w)| w).sum();
            if score > 0.0 && is_definition(lines[i]) {
                score *= 2.0;
            }
            (score > 0.0).then_some((score, i))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let (hits, after): (Vec<usize>, usize) = if ranked.is_empty() {
        (vec![0], lines.len())
    } else {
        (ranked.into_iter().map(|(_, i)| i).collect(), context)
    };

    let mut selected: BTreeSet<usize> = BTreeSet::new();
    let mut used = 0usize;
    'hits: for hit in hits {
        if lines.is_empty() {
            break;
        }
        let start = hit.saturating_sub(context);
        let end = (hit + after + 1).min(lines.len());
        // The matching line first, then its context, so a tight budget keeps the hit.
        for index in std::iter::once(hit).chain(start..end) {
            if selected.contains(&index) || lines[index].trim().is_empty() {
                continue;
            }
            let cost = estimate_tokens(lines[index]);
            if used + cost > token_limit {
                break 'hits;
            }
            selected.insert(index);
            used += cost;
        }
    }
    if selected.is_empty() {
        // One line longer than the whole budget (minified code): hard-truncate it.
        let line = lines.iter().find(|l| !l.trim().is_empty()).copied().unwrap_or("");
        return (line.chars().take(token_limit * 3).collect(), !line.is_empty());
    }

    let mut parts: Vec<String> = Vec::new();
    let mut next = 0usize;
    for &index in &selected {
        if lines[next..index].iter().any(|l| !l.trim().is_empty()) {
            parts.push(format!("@L{}", index + 1));
        }
        parts.push(lines[index].to_string());
        next = index + 1;
    }
    let total = lines.iter().filter(|l| !l.trim().is_empty()).count();
    (parts.join("\n"), selected.len() < total)
}
