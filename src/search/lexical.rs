//! Pinned-tree lexical search.
//!
//! The baseline scanner walks every eligible entity Markdown blob in the
//! pinned commit's tree and returns matches against the query string. It is
//! deliberately simple: no inverted index, no scoring beyond term frequency,
//! no semantic ranking. Callers condense an over-long query before it reaches
//! [`SearchQuery::validate`], then rerank the hits afterwards.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::controls::Controls;
use crate::error::{Error, Result};
use crate::fact::Visibility as FactVis;
use crate::repo::{Blob, GitRepo};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub include_disputed: bool,
    #[serde(default = "default_audience")]
    pub audience: FactVis,
}

fn default_limit() -> usize {
    8
}
fn default_audience() -> FactVis {
    FactVis::Private
}

impl SearchQuery {
    pub fn validate(&self) -> Result<()> {
        let q = self.query.trim();
        if q.is_empty() {
            return Err(Error::InvalidQuery("query must not be empty".into()));
        }
        if q.chars().count() > 200 {
            return Err(Error::InvalidQuery("query exceeds 200 chars".into()));
        }
        if self.limit == 0 || self.limit > 20 {
            return Err(Error::InvalidQuery("limit must be 1..=20".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub fact_id: String,
    pub entity_id: String,
    pub entity_path: String,
    pub statement: String,
    pub score: f32,
    pub sources: Vec<String>,
    pub observed_at: DateTime<Utc>,
    /// Optional N-word snippet of the statement around the first matched term.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Optional excerpts of the source journal events that produced this fact.
    /// Each entry is `(event_id, first-N-lines-of-content)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_excerpts: Option<Vec<SourceExcerpt>>,
}

/// A short excerpt of a source journal event's content, attached to a hit
/// so the caller can see *why* a fact matched without re-running the query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceExcerpt {
    pub event_id: String,
    pub text: String,
}

/// Lexical scan over eligible entity Markdown files in one pinned revision.
pub struct LexicalSearch;

impl LexicalSearch {
    pub fn search(
        repo: &GitRepo,
        revision: &str,
        query: &SearchQuery,
        controls: &Controls,
    ) -> Result<Vec<SearchHit>> {
        query.validate()?;
        let terms: Vec<String> = {
            let mut seen = std::collections::BTreeSet::new();
            tokenize(&query.query)
                .iter()
                .map(|t| stem(t))
                .filter(|t| seen.insert(t.clone()))
                .collect()
        };
        let now = Utc::now();
        let entries: Vec<(String, Blob)> = repo.list_eligible_entity_files(revision)?;
        // Pass 1: eligible facts and the stemmed words of each.
        let mut candidates: Vec<(SearchHit, std::collections::HashSet<String>)> = Vec::new();
        for (path, blob) in entries {
            let entity = match crate::entity::EntityFile::parse(&blob.content) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entity.visibility.allows(query.audience) {
                continue;
            }
            for fact in entity.facts {
                if controls.is_suppressed(&fact.id)
                    || controls.is_retracted(&fact.id)
                    || controls.is_deleted(&fact.id)
                {
                    continue;
                }
                if !fact.visibility.allows(query.audience) {
                    continue;
                }
                if !fact.is_eligible(now, query.include_disputed) {
                    continue;
                }
                let haystack = format!("{} {} {}", fact.statement, entity.title, entity.aliases.join(" "));
                let words = tokenize(&haystack).iter().map(|t| stem(t)).collect();
                candidates.push((
                    SearchHit {
                        fact_id: fact.id.clone(),
                        entity_id: entity.id.clone(),
                        entity_path: path.clone(),
                        statement: fact.statement.clone(),
                        score: 0.0,
                        sources: fact.sources.iter().map(|s| s.event_id.clone()).collect(),
                        observed_at: fact.observed_at,
                        snippet: None,
                        source_excerpts: None,
                    },
                    words,
                ));
            }
        }
        // Pass 2: a matched word counts by how rare it is among eligible
        // facts (inverse document frequency); common words barely count.
        let n = candidates.len() as f32;
        let weights: Vec<f32> = terms
            .iter()
            .map(|t| {
                let df = candidates.iter().filter(|(_, words)| words.contains(t)).count() as f32;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                if STOPWORDS.contains(&t.as_str()) { idf * 0.05 } else { idf }
            })
            .collect();
        let mut hits: Vec<SearchHit> = candidates
            .into_iter()
            .filter_map(|(mut hit, words)| {
                let score: f32 = terms
                    .iter()
                    .zip(&weights)
                    .map(|(t, w)| {
                        if words.contains(t) {
                            *w
                        } else if words.iter().any(|word| shares_prefix(t, word)) {
                            // licence/license, config/configuration: related
                            // spellings count for less than an exact stem.
                            *w * 0.6
                        } else {
                            0.0
                        }
                    })
                    .sum();
                (score > 0.0).then(|| {
                    hit.score = score;
                    hit
                })
            })
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(query.limit);
        Ok(hits)
    }
}

/// Words that say little about what a fact is about.
pub const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "do", "doe",
    "does", "for", "from", "has", "have", "how", "i", "in", "is", "it", "of", "on", "or", "should",
    "that", "the", "thi", "this", "to", "was", "we", "what", "when", "where", "which", "who", "why",
    "with", "work", "under", "over", "into", "onto", "via", "within", "without", "between",
    "after", "before", "than", "then", "there", "their", "them", "they", "its", "also", "just",
    "only", "all", "any", "some", "each", "every", "more", "most", "other", "such", "same", "so",
    "if", "else", "up", "out", "off", "my", "our", "your", "you", "me", "us", "use", "need",
];

/// Light suffix stripping so "agents"/"agent" and "deploys"/"deployed"
/// match. Short words are left alone.
pub fn stem(word: &str) -> String {
    let w = word.to_lowercase();
    for suffix in ["ing", "ed"] {
        if let Some(root) = w.strip_suffix(suffix) {
            if root.chars().count() >= 3 {
                return root.to_string();
            }
        }
    }
    // "-es" only after a sibilant (pushes, classes, boxes); otherwise just
    // "-s" (changes -> change, uses -> use). "ss" words keep their "s".
    if let Some(root) = w.strip_suffix("es") {
        if root.chars().count() >= 3 && ["s", "x", "z", "ch", "sh"].iter().any(|e| root.ends_with(e)) {
            return root.to_string();
        }
    }
    if let Some(root) = w.strip_suffix('s') {
        if root.chars().count() >= 3 && !root.ends_with('s') {
            return root.to_string();
        }
    }
    w
}

/// Two stems of at least five characters that begin with the same five.
fn shares_prefix(a: &str, b: &str) -> bool {
    const N: usize = 5;
    a.len() >= N && b.len() >= N && a.is_char_boundary(N) && b.is_char_boundary(N) && a[..N] == b[..N]
}

pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Build a `words`-long snippet of `haystack` centred on the first occurrence
/// of any `terms` (case-insensitive). Returns the full haystack when it is
/// shorter than the requested window. Prepends / appends `...` when truncated.
pub fn snippet_around_terms(haystack: &str, terms: &[String], words: usize) -> String {
    if words == 0 || haystack.is_empty() {
        return String::new();
    }
    let lowered = haystack.to_lowercase();
    let mut hit_pos: Option<usize> = None;
    for term in terms {
        if let Some(p) = lowered.find(&term.to_lowercase()) {
            hit_pos = match hit_pos {
                Some(prev) => Some(prev.min(p)),
                None => Some(p),
            };
        }
    }
    let haystack_words: Vec<&str> = haystack.split_whitespace().collect();
    if haystack_words.len() <= words {
        return haystack.to_string();
    }
    let centre_word = match hit_pos {
        Some(p) => {
            // Locate the word containing position `p` by walking the haystack.
            let mut acc = 0usize;
            let mut idx = 0usize;
            for (i, w) in haystack_words.iter().enumerate() {
                let next = acc + w.len();
                if p >= acc && p <= next {
                    idx = i;
                    break;
                }
                // +1 for the space between words
                acc = next + 1;
                if p < acc {
                    idx = i;
                    break;
                }
            }
            idx
        }
        None => haystack_words.len() / 2,
    };
    let half = words / 2;
    let start = centre_word.saturating_sub(half);
    let end = (start + words).min(haystack_words.len());
    let actual_start = if end - start < words && start > 0 {
        end.saturating_sub(words)
    } else {
        start
    };
    let mut out = haystack_words[actual_start..end].join(" ");
    if actual_start > 0 {
        out = format!("... {out}");
    }
    if end < haystack_words.len() {
        out.push_str(" ...");
    }
    out
}

/// Return the first `lines` non-empty lines of `content`, with trailing `...`
/// if there is more.
pub fn excerpt_lines(content: &str, lines: usize) -> String {
    if lines == 0 || content.is_empty() {
        return String::new();
    }
    let mut taken = 0usize;
    let mut out = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if taken > 0 {
            out.push('\n');
        }
        out.push_str(trimmed);
        taken += 1;
        if taken >= lines {
            break;
        }
    }
    if taken == 0 {
        // All lines were blank — return the original (trimmed) text.
        return content.trim().to_string();
    }
    // Count remaining non-empty lines to decide whether to add `...`.
    let mut remaining = 0usize;
    for line in content.lines() {
        if !line.trim().is_empty() {
            remaining += 1;
        }
    }
    if remaining > taken {
        out.push_str("\n...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stemming_joins_plural_and_tense() {
        assert_eq!(stem("agents"), stem("agent"));
        assert_eq!(stem("deploys"), stem("deployed"));
        assert_eq!(stem("streaming"), "stream");
        assert_eq!(stem("is"), "is");
        assert_eq!(stem("changes"), stem("change"));
        assert_eq!(stem("pushes"), stem("push"));
        assert_eq!(stem("classes"), "class");
        assert_eq!(stem("class"), "class");
    }

    #[test]
    fn related_spellings_share_a_prefix() {
        assert!(shares_prefix(&stem("licence"), &stem("License")));
        assert!(shares_prefix(&stem("config"), &stem("configuration")));
        assert!(!shares_prefix(&stem("agent"), &stem("agensis")));
        assert!(!shares_prefix("fly", "flyer"));
    }

    #[test]
    fn tokenize_splits_on_punctuation() {
        assert_eq!(
            tokenize("Hello, world! Foo-Bar"),
            vec!["hello", "world", "foo-bar"]
        );
    }

    #[test]
    fn query_validation() {
        let q = SearchQuery {
            query: "   ".into(),
            limit: 5,
            include_disputed: false,
            audience: FactVis::Private,
        };
        assert!(q.validate().is_err());

        let q = SearchQuery {
            query: "x".into(),
            limit: 0,
            include_disputed: false,
            audience: FactVis::Private,
        };
        assert!(q.validate().is_err());

        let q = SearchQuery {
            query: "x".into(),
            limit: 5,
            include_disputed: false,
            audience: FactVis::Private,
        };
        assert!(q.validate().is_ok());

        // The cap is characters. Two hundred é characters are 400 bytes and
        // still a legal query; one more ASCII character is not.
        let q = SearchQuery {
            query: "é".repeat(200),
            limit: 5,
            include_disputed: false,
            audience: FactVis::Private,
        };
        assert!(q.validate().is_ok());
        let q = SearchQuery {
            query: "a".repeat(201),
            limit: 5,
            include_disputed: false,
            audience: FactVis::Private,
        };
        assert!(q.validate().is_err());
    }

    #[test]
    fn snippet_around_first_match() {
        let s = "Alex knows Rust, Python, and TypeScript well; he has shipped systems in each language for years and likes async runtimes.";
        let terms = vec!["rust".into(), "alex".into()];
        let snip = snippet_around_terms(s, &terms, 5);
        assert!(snip.starts_with("...") || snip.to_lowercase().contains("alex"));
        assert!(snip.split_whitespace().count() <= 5 + 2); // ... and ellipsis
        assert!(snip.ends_with("..."));
    }

    #[test]
    fn snippet_handles_short_input() {
        let s = "Alex likes Rust.";
        let terms = vec!["rust".into()];
        let snip = snippet_around_terms(s, &terms, 20);
        assert_eq!(snip, "Alex likes Rust.");
    }

    #[test]
    fn excerpt_lines_limits_and_truncates() {
        let s = "alpha\n\nbeta\ngamma\ndelta\nepsilon";
        assert_eq!(excerpt_lines(s, 3), "alpha\nbeta\ngamma\n...");
        assert_eq!(excerpt_lines(s, 10), "alpha\nbeta\ngamma\ndelta\nepsilon");
        assert_eq!(excerpt_lines(s, 0), "");
        assert_eq!(excerpt_lines("", 5), "");
    }
}