//! Lexical scan over journal events.
//!
//! [`LexicalSearch`] only walks the published entity tree, so a freshly
//! ingested session is invisible until `consolidate` has produced facts.
//! [`JournalSearch`] reads the journal directly so the user can find what
//! they just ingested without waiting for (or trusting) the extraction step.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::journal::{Journal, JournalEvent, Role};
use crate::search::lexical::tokenize;

/// A single hit against the journal. Shaped like [`SearchHit`] so callers
/// can mix journal and entity hits in one result list, but the
/// `entity_id` / `entity_path` fields carry the source provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalSearchHit {
    pub event_id: String,
    pub seq: u64,
    pub scope_id: String,
    pub session_id: String,
    pub role: Role,
    pub source_kind: String,
    /// First 200 chars of the matched event content — gives the caller
    /// enough to decide whether to read the full event.
    pub excerpt: String,
    /// Event content kept for the result. Capped at [`KEPT_CHARS`] so a
    /// multi-megabyte row cannot sit in the result list.
    pub content: String,
    pub score: f32,
    pub occurred_at: Option<DateTime<Utc>>,
    pub ingested_at: DateTime<Utc>,
}

pub struct JournalSearch;

/// How many short rows that share a hit's event id are kept.
const SIBLINGS_PER_ID: usize = 8;
/// Short rows swapped onto a full result page so a buried turn stays visible.
const SIBLINGS_ON_PAGE: usize = 2;
/// Short sibling rows only. The long row is already a lexical hit.
const SIBLING_MAX_CHARS: usize = 2_000;
/// Characters of a kept row. The scan still scores the full text.
const KEPT_CHARS: usize = 2_048;
/// Nearby short rows remembered so a turn just before a top hit can attach.
const RECENT_SHORTS: usize = 256;

/// Common words. They still match, at a low weight, so a rare word decides
/// the order. No corpus index: the list is fixed.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "for", "from", "has", "have",
    "i", "in", "is", "it", "of", "on", "or", "that", "the", "this", "to", "user", "was", "were",
    "with",
];

/// Lexical hits plus how many journal rows were read to produce them.
pub struct JournalScan {
    pub hits: Vec<JournalSearchHit>,
    pub scanned: u64,
}

impl JournalSearch {
    /// Scan every journal event under `journal_dir` for `query`. Returns the
    /// top `limit` hits by summed term-frequency score.
    pub fn search(
        journal_dir: &std::path::Path,
        query: &str,
        limit: usize,
        scope_filter: Option<&str>,
        source_filter: &[String],
    ) -> Result<Vec<JournalSearchHit>> {
        Ok(Self::search_detailed(
            journal_dir,
            query,
            limit,
            scope_filter,
            source_filter,
            None,
            false,
        )?
        .hits)
    }

    /// Like [`search`](Self::search), but also reports how many events were
    /// read. When `siblings` is set, each top hit also pulls in other short
    /// rows that share its event id. Ingested transcripts often reuse one id
    /// for a whole conversation, so a multi-megabyte row outscores the short
    /// turn next to it ("Prompt is too long") that never repeats the query words.
    pub fn search_detailed(
        journal_dir: &std::path::Path,
        query: &str,
        limit: usize,
        scope_filter: Option<&str>,
        source_filter: &[String],
        project_filter: Option<&str>,
        siblings: bool,
    ) -> Result<JournalScan> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(JournalScan {
                hits: Vec::new(),
                scanned: 0,
            });
        }
        let journal = Journal::open(journal_dir)?;
        let terms = tokenize(query);
        let mut heap: BinaryHeap<RankedHit> = BinaryHeap::new();
        let mut tracked: HashSet<String> = HashSet::new();
        let mut shorts: HashMap<String, Vec<JournalSearchHit>> = HashMap::new();
        let mut recent: VecDeque<JournalSearchHit> = VecDeque::with_capacity(RECENT_SHORTS);
        let mut scanned = 0u64;
        for event in journal.iter()? {
            let event = event?;
            scanned += 1;
            if !passes_filters(&event, scope_filter, source_filter, project_filter) {
                continue;
            }
            let score = score_text(&event, &terms);
            if score > 0.0 && enters_top(&heap, score, event.seq, limit) {
                let hit = hit_from_event(&event, &terms, score);
                push_top(&mut heap, hit, limit);
                note_tracked(&heap, &mut tracked, &mut shorts, &recent);
            }
            if event.content.chars().count() <= SIBLING_MAX_CHARS {
                let short = sibling_hit(&event);
                if tracked.contains(&event.event_id) {
                    remember_short(&mut shorts, short.clone());
                }
                if recent.len() == RECENT_SHORTS {
                    recent.pop_front();
                }
                recent.push_back(short);
            }
        }

        let mut hits = heap_to_vec(heap);
        if siblings && !hits.is_empty() {
            let mut extra = Vec::new();
            // Only a row that was cut off. Short neighbors of an already-short
            // hit are the next chat lines, not a buried turn.
            for id in hits
                .iter()
                .take(3)
                .filter(|hit| hit.content.ends_with('…'))
                .map(|hit| hit.event_id.clone())
            {
                let Some(list) = shorts.remove(&id) else {
                    continue;
                };
                for short in list {
                    if hits.iter().any(|hit| hit.seq == short.seq) {
                        continue;
                    }
                    // One-word acknowledgements ("works", "continue") are not
                    // the buried turn this is here to recover.
                    if short.content.trim().chars().count() < 16 {
                        continue;
                    }
                    extra.push(short);
                }
            }
            extra.sort_by_key(|hit| hit.content.len());
            extra.truncate(SIBLINGS_ON_PAGE);
            // Appended after the ranked rows. A caller that asked for `limit`
            // rows keeps the ranked ones and drops these unless it asked for
            // them (the JEV pool reads past `limit`).
            hits.extend(extra);
        }

        Ok(JournalScan { hits, scanned })
    }
}

fn enters_top(heap: &BinaryHeap<RankedHit>, score: f32, seq: u64, cap: usize) -> bool {
    if cap == 0 {
        return false;
    }
    if heap.len() < cap {
        return true;
    }
    heap.peek()
        .map(|worst| hit_beats(score, seq, worst.0.score, worst.0.seq))
        .unwrap_or(true)
}

fn hit_beats(score: f32, seq: u64, other_score: f32, other_seq: u64) -> bool {
    match score.partial_cmp(&other_score).unwrap_or(std::cmp::Ordering::Equal) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => seq < other_seq,
    }
}

fn push_top(heap: &mut BinaryHeap<RankedHit>, hit: JournalSearchHit, cap: usize) {
    if heap.len() >= cap {
        heap.pop();
    }
    heap.push(RankedHit(hit));
}

fn note_tracked(
    heap: &BinaryHeap<RankedHit>,
    tracked: &mut HashSet<String>,
    shorts: &mut HashMap<String, Vec<JournalSearchHit>>,
    recent: &VecDeque<JournalSearchHit>,
) {
    let mut now = HashSet::new();
    for item in heap.iter() {
        now.insert(item.0.event_id.clone());
    }
    for id in now.iter() {
        if tracked.insert(id.clone()) {
            for short in recent.iter().filter(|short| &short.event_id == id) {
                remember_short(shorts, short.clone());
            }
        }
    }
    shorts.retain(|id, _| now.contains(id));
    *tracked = now;
}

fn remember_short(shorts: &mut HashMap<String, Vec<JournalSearchHit>>, hit: JournalSearchHit) {
    let list = shorts.entry(hit.event_id.clone()).or_default();
    if list.iter().any(|kept| kept.seq == hit.seq) {
        return;
    }
    list.push(hit);
    list.sort_by_key(|kept| kept.content.len());
    list.truncate(SIBLINGS_PER_ID);
}

fn heap_to_vec(heap: BinaryHeap<RankedHit>) -> Vec<JournalSearchHit> {
    let mut hits: Vec<JournalSearchHit> = heap.into_iter().map(|ranked| ranked.0).collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.seq.cmp(&b.seq))
    });
    hits
}

/// Lowest score, then highest seq, so [`BinaryHeap`] pops the row to drop.
struct RankedHit(JournalSearchHit);

impl PartialEq for RankedHit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for RankedHit {}
impl PartialOrd for RankedHit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RankedHit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match other
            .0
            .score
            .partial_cmp(&self.0.score)
            .unwrap_or(std::cmp::Ordering::Equal)
        {
            std::cmp::Ordering::Equal => self.0.seq.cmp(&other.0.seq),
            ord => ord,
        }
    }
}

/// True when the event's recorded project is `project`, given either as the
/// project's folder name or as its root path.
pub fn project_matches(event: &JournalEvent, project: &str) -> bool {
    let project = project.trim_end_matches('/');
    match (event.project(), event.project_name()) {
        (Some(path), Some(name)) => name.eq_ignore_ascii_case(project) || path == project,
        _ => false,
    }
}

fn passes_filters(
    event: &JournalEvent,
    scope_filter: Option<&str>,
    source_filter: &[String],
    project_filter: Option<&str>,
) -> bool {
    if let Some(scope) = scope_filter {
        if event.scope_id != scope {
            return false;
        }
    }
    if let Some(project) = project_filter {
        if !project_matches(event, project) {
            return false;
        }
    }
    if source_filter.is_empty() {
        return true;
    }
    source_filter.iter().any(|p| {
        event
            .source
            .event_id()
            .to_lowercase()
            .starts_with(&p.to_lowercase())
            || match &event.source {
                crate::journal::Source::Chat { source_id, .. } => {
                    source_id.to_lowercase().starts_with(&p.to_lowercase())
                }
                _ => false,
            }
    })
}

/// Score content and metadata. Metadata stays searchable so a dataset field
/// such as choices or labels can match without a second store.
fn score_text(event: &JournalEvent, terms: &[String]) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let mut weighted = 0f32;
    let mut bytes = 0usize;
    accumulate(&event.content, terms, &mut weighted, &mut bytes);
    if !event.metadata.is_null() {
        let meta = serde_json::to_string(&event.metadata).unwrap_or_default();
        accumulate(&meta, terms, &mut weighted, &mut bytes);
    }
    if weighted <= 0.0 || bytes == 0 {
        return 0.0;
    }
    // Floor the length so a 20-character row does not outrank a normal note
    // that says the same word. Rows past this still get pushed down.
    let norm = (bytes.max(2_000) as f32).ln();
    weighted / norm
}

fn accumulate(text: &str, terms: &[String], weighted: &mut f32, bytes: &mut usize) {
    if text.is_empty() {
        return;
    }
    *bytes += text.len();
    let lower = text.to_lowercase();
    for term in terms {
        let mut tf = 0u32;
        let mut search_pos = 0usize;
        // `match_indices` is char-boundary-safe.
        for (start, _) in lower.match_indices(term.as_str()) {
            if start < search_pos {
                continue;
            }
            tf += 1;
            search_pos = start + term.len();
            if search_pos >= lower.len() {
                break;
            }
        }
        if tf > 0 {
            *weighted += term_weight(term) * (1.0 + tf as f32).ln();
        }
    }
}

fn term_weight(term: &str) -> f32 {
    if STOPWORDS.binary_search(&term).is_ok() {
        0.05
    } else {
        1.0
    }
}

fn hit_from_event(event: &JournalEvent, terms: &[String], score: f32) -> JournalSearchHit {
    JournalSearchHit {
        event_id: event.event_id.clone(),
        seq: event.seq,
        scope_id: event.scope_id.clone(),
        session_id: event.session_id.clone(),
        role: event.role,
        source_kind: source_kind_label(event),
        excerpt: make_excerpt(&event.content, terms),
        content: clip_chars(&event.content, KEPT_CHARS),
        score,
        occurred_at: Some(event.occurred_at),
        ingested_at: event.ingested_at,
    }
}

fn sibling_hit(event: &JournalEvent) -> JournalSearchHit {
    let content = clip_chars(&event.content, KEPT_CHARS);
    JournalSearchHit {
        event_id: event.event_id.clone(),
        seq: event.seq,
        scope_id: event.scope_id.clone(),
        session_id: event.session_id.clone(),
        role: event.role,
        source_kind: source_kind_label(event),
        excerpt: content.clone(),
        content,
        score: 0.0,
        occurred_at: Some(event.occurred_at),
        ingested_at: event.ingested_at,
    }
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let mut end = text.len();
    for (i, (idx, _)) in text.char_indices().enumerate() {
        if i == max_chars {
            end = idx;
            break;
        }
    }
    let mut out = text[..end].to_string();
    out.push('…');
    out
}

fn make_excerpt(content: &str, terms: &[String]) -> String {
    if content.is_empty() {
        return String::new();
    }
    const WINDOW: usize = 80;
    const RADIUS: usize = 200;
    let lower = content.to_lowercase();
    let mut best_pos: Option<usize> = None;
    for term in terms {
        if let Some(p) = lower.find(&term.to_lowercase()) {
            best_pos = match best_pos {
                Some(prev) => Some(prev.min(p)),
                None => Some(p),
            };
        }
    }
    let centre = best_pos.unwrap_or(0);
    // Round down to the nearest char boundary — the matched position may be
    // inside a multi-byte char.
    let mut start = centre.saturating_sub(RADIUS);
    while start > 0 && !content.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + WINDOW * 4).min(content.len());
    while end < content.len() && !content.is_char_boundary(end) {
        end += 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push_str("... ");
    }
    out.push_str(&content[start..end]);
    if end < content.len() {
        out.push_str(" ...");
    }
    out
}

fn source_kind_label(event: &JournalEvent) -> String {
    match &event.source {
        crate::journal::Source::Chat { source_id, .. } => {
            // Chat source_id is something like `claude:<uuid>`. Use the prefix
            // so filtering by `--source claude:` works at journal level too.
            source_id
                .split(':')
                .next()
                .unwrap_or("chat")
                .to_string()
        }
        crate::journal::Source::Calendar { .. } => "calendar".into(),
        crate::journal::Source::Voice { .. } => "voice".into(),
        crate::journal::Source::IdeHistory { .. } => "ide".into(),
        crate::journal::Source::Custom { custom_kind, .. } => custom_kind.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Journal, JournalEvent, Redaction, Role, Source};

    fn make_event(scope: &str, content: &str) -> JournalEvent {
        JournalEvent::new(
            scope,
            "session_x",
            Role::User,
            Source::Chat {
                source_id: "claude:abc".into(),
                occurred_at: None,
            },
            content,
            Redaction::None,
        )
    }

    #[test]
    fn finds_matching_event() {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(tmp.path()).unwrap();
        for content in [
            "the quick brown fox jumps over the lazy dog",
            "no match here at all whatsoever",
            "another line entirely unrelated",
            "fox news is unrelated but the word fox appears",
        ] {
            j.append(make_event("personal", content)).unwrap();
        }
        let hits = JournalSearch::search(tmp.path(), "fox", 10, None, &[]).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].content.to_lowercase().contains("fox"));
    }

    #[test]
    fn respects_scope_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(tmp.path()).unwrap();
        j.append(make_event("work", "rust async runtime")).unwrap();
        j.append(make_event("personal", "rust hobby project")).unwrap();
        let hits = JournalSearch::search(tmp.path(), "rust", 10, Some("work"), &[]).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope_id, "work");
    }

    #[test]
    fn siblings_surface_a_short_turn_that_shares_the_long_rows_id() {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(tmp.path()).unwrap();
        let mut long = make_event(
            "personal",
            &"the user has been with the things ".repeat(80),
        );
        long.event_id = "evt_chat_claude:shared".into();
        let mut prompt = make_event("personal", "Prompt is too long");
        prompt.event_id = long.event_id.clone();
        j.append(long).unwrap();
        j.append(prompt).unwrap();
        j.append(make_event(
            "personal",
            "race condition crashes browser LivePreview.tsx:269 The above error occurred in the component.",
        ))
        .unwrap();

        let lexical = JournalSearch::search(
            tmp.path(),
            "things the user has been frustrated with",
            5,
            None,
            &[],
        )
        .unwrap();
        assert!(
            lexical.iter().all(|h| h.content != "Prompt is too long"),
            "lexical term frequency cannot see a short turn with no query words"
        );

        let scan = JournalSearch::search_detailed(
            tmp.path(),
            "things the user has been frustrated with",
            5,
            None,
            &[],
            None,
            true,
        )
        .unwrap();
        assert!(scan.scanned >= 3);
        assert!(
            scan.hits.iter().any(|h| h.content == "Prompt is too long"),
            "short sibling of the long transcript must enter the rerank pool"
        );
    }

    #[test]
    fn rare_word_beats_a_row_stuffed_with_common_words() {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(tmp.path()).unwrap();
        let stuffed = format!("{} frustrated", "the user with ".repeat(4_000));
        j.append(make_event("personal", &stuffed)).unwrap();
        j.append(make_event(
            "personal",
            "URGENT, user frustrated: no towers",
        ))
        .unwrap();
        let hits = JournalSearch::search(tmp.path(), "frustrated", 5, None, &[]).unwrap();
        assert!(hits[0].content.contains("no towers"), "{}", hits[0].content);
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn kept_row_is_capped_and_excerpt_still_sees_the_match() {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(tmp.path()).unwrap();
        let mut body = "a".repeat(8_000);
        body.push_str(" fox ");
        j.append(make_event("personal", &body)).unwrap();
        let hits = JournalSearch::search(tmp.path(), "fox", 5, None, &[]).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.chars().count() <= KEPT_CHARS + 1);
        assert!(hits[0].excerpt.contains("fox"));
    }
}
