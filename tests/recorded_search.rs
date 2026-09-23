//! Recorded checks from the 2026-09-21 session on the real journal.
//!
//! Two separate contracts:
//! - lexical search returns events that contain the query words, ranked by
//!   how often those words occur;
//! - the local cross-encoder, given those kinds of documents, ranks a
//!   user-visible failure above an unrelated success even when the failure
//!   text never uses the word "frustrated".

use mem::journal::{Journal, JournalEvent, Redaction, Role, Source};
use mem::search::journal::JournalSearch;
use mem::search::{LayaReranker, ModelKind, Reranker, SearchHit};

const PROMPT_TOO_LONG: &str = "\
Error from the model: Prompt is too long. The request was rejected and the turn ended.";

const BROWSER_CRASH: &str = "\
race condition crashes browser LivePreview.tsx:269. The above error occurred in the Layer3 component. \
React will try to recreate this component tree from scratch using the error boundary.";

const DEVTOOLS_NAG: &str = "\
Download the React DevTools for a better development experience: https://react.dev/link/react-devtools";

const TAILWIND_WARNING: &str = "\
cdn.tailwindcss.com should not be used in production. To use Tailwind CSS in production, \
install it as a PostCSS plugin or use the Tailwind CLI: https://tailwindcss.com/docs/installation";

const CANVAS_LOG: &str = "\
Canvas page mounted. log: Current code. log: Current theme: light. log: Components available: 12356. \
Canvas received message: [object Object]";

const UNRELATED_SUCCESS: &str = "\
The pull request was merged. CI passed.";

const USER_SAID_FRUSTRATED: &str = "URGENT, user frustrated: no towers";

const USER_WANTS_VERIFICATION: &str = "when frustrated, user wants verification before the next change";

const REPEATED_FRUSTRATED: &str = "\
debugging methodology note. if the user is frustrated, reproduce first. \
the word frustrated appears in the rubric. frustrated. frustrated. frustrated.";

fn chat_event(content: &str) -> JournalEvent {
    JournalEvent::new(
        "personal",
        "session_x",
        Role::User,
        Source::Chat {
            source_id: "claude:recorded".into(),
            occurred_at: None,
        },
        content,
        Redaction::None,
    )
}

fn hit(id: &str, statement: &str) -> SearchHit {
    SearchHit {
        fact_id: id.into(),
        entity_id: "journal".into(),
        entity_path: "journal/claude".into(),
        statement: statement.into(),
        score: 0.0,
        sources: vec![id.into()],
        observed_at: chrono::Utc::now(),
        snippet: None,
        source_excerpts: None,
    }
}

fn contains_frustrated(text: &str) -> bool {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .any(|w| w == "frustrated")
}

#[test]
fn lexical_frustrated_returns_only_word_matches_and_ranks_repetition_first() {
    let tmp = tempfile::tempdir().unwrap();
    let journal = Journal::open(tmp.path()).unwrap();
    for content in [
        PROMPT_TOO_LONG,
        BROWSER_CRASH,
        DEVTOOLS_NAG,
        TAILWIND_WARNING,
        CANVAS_LOG,
        UNRELATED_SUCCESS,
        USER_SAID_FRUSTRATED,
        USER_WANTS_VERIFICATION,
        REPEATED_FRUSTRATED,
    ] {
        journal.append(chat_event(content)).unwrap();
    }

    let hits = JournalSearch::search(tmp.path(), "frustrated", 10, None, &[]).unwrap();
    assert!(
        hits.len() >= 3,
        "expected the three events that actually say frustrated, got {}",
        hits.len()
    );
    assert!(
        hits.iter().all(|h| contains_frustrated(&h.content)),
        "lexical hit missing the word: {:?}",
        hits.iter().map(|h| h.content.as_str()).collect::<Vec<_>>()
    );
    assert!(
        !hits.iter().any(|h| h.content.contains("Prompt is too long")),
        "a failure that never says frustrated must not be a lexical hit"
    );
    assert!(
        !hits.iter().any(|h| h.content.contains("LivePreview.tsx")),
        "the browser crash must not be a lexical hit for the bare word"
    );
    assert!(
        hits[0].content.contains("debugging methodology"),
        "term frequency should put the repeated wording first, got: {}",
        hits[0].excerpt
    );
    assert!(hits[0].score > hits[1].score);
}

#[test]
#[ignore = "needs the local model: `mem models pull`, then `cargo test -- --ignored`"]
fn laya_separates_a_direct_answer_from_an_unrelated_sentence() {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mem/.models");
    let reranker = LayaReranker::try_new(&cache, ModelKind::BgeRerankerBase)
        .expect("reranker init")
        .expect("bge-reranker-base is cached under mem/.models");

    let out = reranker
        .rerank(
            "what is the capital of France",
            vec![
                hit("cat", "The cat sat on the mat."),
                hit("paris", "Paris is the capital of France."),
            ],
            2,
        )
        .expect("rerank");
    assert!(out.judged);
    assert!(
        out.hits[0].statement.contains("Paris"),
        "direct answer should rank first, got {}",
        out.hits[0].statement
    );
    assert!(out.hits[0].score > 0.5, "paris score {}", out.hits[0].score);
    assert!(
        out.hits[1].score < 0.1,
        "unrelated sentence score {}",
        out.hits[1].score
    );
}

#[test]
#[ignore = "needs the local model: `mem models pull`, then `cargo test -- --ignored`"]
fn laya_keeps_lexical_order_when_every_candidate_is_irrelevant() {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mem/.models");
    let reranker = LayaReranker::try_new(&cache, ModelKind::BgeRerankerBase)
        .expect("reranker init")
        .expect("bge-reranker-base is cached under mem/.models");
    let out = reranker
        .rerank(
            "things the user has been frustrated with",
            vec![
                hit("first", "Canvas page mounted. log: Current code."),
                hit("second", "cdn.tailwindcss.com should not be used in production."),
            ],
            2,
        )
        .expect("rerank");
    assert!(!out.judged, "scores: {:?}", out.hits.iter().map(|h| h.score).collect::<Vec<_>>());
    assert_eq!(out.hits[0].fact_id, "first");
    assert_eq!(out.hits[1].fact_id, "second");
}
