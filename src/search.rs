//! In-memory lexical search (SUR-744) — a faithful port of the PWA's MiniSearch engine
//! (`surfc/src/lib/lexicalSearch.js`, SUR-527). Native-only, like `store`/`sync`: the PWA
//! keeps its own MiniSearch on wasm.
//!
//! ## Why a hand-rolled index and not FTS5
//!
//! The gate (AC #5 / SUR-754) is **verdict parity with the PWA** — same hit/miss/diacritics/
//! stemming/prefix decisions on the same fixtures. MiniSearch matches on a custom ~20-line
//! `stem()` (ported verbatim below), `prefix: true`, and `fuzzy: 0.2` (Levenshtein). SQLite
//! FTS5 has prefix matching but **no custom-stemmer hook** (its Porter stemmer diverges from
//! `stem()` on the first `-ing`) and **no fuzzy** — so it cannot reproduce the verdicts. A
//! small inverted-free linear index over the already-decrypted docs does, and is `:memory:`
//! by construction, so no plaintext note text ever reaches disk (AC #4, decision 3).
//!
//! ## Parity boundary (recorded in ADR 0005)
//!
//! **Verdicts are exact; ranking is a faithful approximation.** The matcher reproduces
//! MiniSearch's observable matching — tokenize on `\p{Z}\p{P}` + CR/LF, `stem()` on every
//! indexed and query term, then exact ∪ prefix ∪ fuzzy(Levenshtein ≤ `min(6, round(len·0.2))`)
//! OR-combined, with the title field boosted 2× and a `quality` multiplier (number of distinct
//! query terms a doc matched). It does NOT reproduce MiniSearch's exact BM25 term-frequency
//! saturation — only the relative ordering properties the screens rely on (title > body, more
//! query-terms-matched ranks higher, exact > prefix > fuzzy). No SUR-754 parity case pins an
//! exact score or a full ordering, so this is a deliberate, documented deviation.
//!
//! ## Lifecycle
//!
//! Rebuilt per `search()` call from the live store (scan → decrypt → index → query → discard).
//! No cached index state on `SyncEngine`, no invalidation threaded through the write/sync paths.
//! Bounded at personal-archive scale (hundreds–low-thousands of rows = milliseconds).
//! ponytail: cache + incrementally feed the index at `enqueue`/`pull` time only if profiling
//! ever shows the per-search rebuild matters.

/// Which entity a [`SearchHit`] points at. Mirrors the PWA's `type` field (`'note'`/`'idea'`),
/// but a closed enum gives Swift/Kotlin an exhaustive switch instead of a stringly-typed field.
/// Scope is notes + custom-ideas only (SUR-744 decision 1); books aren't indexed by the PWA and
/// lenses/collections have no v1 read surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SearchDocKind {
    Note,
    Idea,
}

/// One search result, shaped like the PWA's `runSearch` output: `refId → ref_id`, `type → kind`,
/// plus `title`/`snippet`/`score`. `snippet` is the content (or the title when the doc has no
/// body), matching `hit.content || hit.title`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SearchHit {
    pub kind: SearchDocKind,
    pub ref_id: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

/// An already-**decrypted** document handed to the indexer. Built one layer up (`sync::read`)
/// from live store rows, mirroring the PWA indexing its in-memory decrypted arrays — never
/// ciphertext. `title` is the boosted field (idea name; empty for notes), `content` the body
/// (note text / idea description).
pub struct SearchDoc {
    pub kind: SearchDocKind,
    pub ref_id: String,
    pub title: String,
    pub content: String,
}

// MiniSearch default weights (unmodified by lexicalSearch.js's SEARCH_OPTIONS, which only sets
// boost/fuzzy/prefix): exact term weight 1, prefix 0.375, fuzzy 0.45; title field boost 2.
const PREFIX_WEIGHT: f64 = 0.375;
const FUZZY_WEIGHT: f64 = 0.45;
const TITLE_BOOST: f64 = 2.0;
const CONTENT_BOOST: f64 = 1.0;
/// `fuzzy: 0.2` — allowed edit distance is `round(term.len * 0.2)`, capped at MiniSearch's
/// `maxFuzzy` default of 6.
const FUZZY_RATIO: f64 = 0.2;
const MAX_FUZZY: usize = 6;

/// Run a query over the decrypted docs and return up to `limit` hits, best-first. An empty or
/// whitespace-only query returns `[]` (the PWA's "no search-everything surprise").
pub fn search(docs: &[SearchDoc], query: &str, limit: usize) -> Vec<SearchHit> {
    let query_terms = distinct(stem_all(query));
    if query_terms.is_empty() {
        return vec![];
    }

    // Pre-stem each doc's two fields once.
    struct Indexed<'a> {
        doc: &'a SearchDoc,
        title_terms: Vec<String>,
        content_terms: Vec<String>,
    }
    let indexed: Vec<Indexed> = docs
        .iter()
        .map(|doc| Indexed {
            doc,
            title_terms: stem_all(&doc.title),
            content_terms: stem_all(&doc.content),
        })
        .collect();

    let mut scored: Vec<(f64, usize)> = Vec::new();
    for (i, entry) in indexed.iter().enumerate() {
        let mut score = 0.0f64;
        let mut matched = 0u32; // distinct query terms this doc matched (the `quality` multiplier)
        for qt in &query_terms {
            let title_w = best_field_weight(qt, &entry.title_terms) * TITLE_BOOST;
            let content_w = best_field_weight(qt, &entry.content_terms) * CONTENT_BOOST;
            let contribution = title_w.max(content_w);
            if contribution > 0.0 {
                score += contribution;
                matched += 1;
            }
        }
        if matched > 0 {
            score *= matched as f64; // OR combinator's quality boost: reward matching more terms
            scored.push((score, i));
        }
    }

    // Descending by score; `sort_by` is stable, so ties keep insertion order (notes, then ideas —
    // the same order the PWA's Map iteration preserves).
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
        .into_iter()
        .map(|(score, i)| {
            let d = indexed[i].doc;
            SearchHit {
                kind: d.kind,
                ref_id: d.ref_id.clone(),
                title: d.title.clone(),
                snippet: if d.content.is_empty() {
                    d.title.clone()
                } else {
                    d.content.clone()
                },
                score,
            }
        })
        .collect()
}

/// The best match weight for a query term against one field's indexed terms: exact (1.0) beats
/// prefix beats fuzzy, and — like MiniSearch — a term counted as a prefix match is never also
/// counted as fuzzy. `0.0` = no match. Aggregated as the max across the field's terms (a faithful
/// stand-in for MiniSearch's per-term BM25 sum; see the module's parity-boundary note).
fn best_field_weight(qt: &str, terms: &[String]) -> f64 {
    let max_distance = fuzzy_max_distance(qt);
    let qt_len = qt.chars().count();
    let mut best = 0.0f64;
    for it in terms {
        let w = if it.as_str() == qt {
            1.0
        } else if it.starts_with(qt) {
            let it_len = it.chars().count() as f64;
            let gap = (it.chars().count() - qt_len) as f64;
            PREFIX_WEIGHT * it_len / (it_len + 0.3 * gap)
        } else {
            let d = levenshtein(qt, it);
            if d > 0 && d <= max_distance {
                let it_len = it.chars().count() as f64;
                FUZZY_WEIGHT * it_len / (it_len + d as f64)
            } else {
                0.0
            }
        };
        if w > best {
            best = w;
        }
    }
    best
}

/// `min(maxFuzzy, round(len · fuzzy))` on the (stemmed) query term's length. `f64::round` is
/// half-away-from-zero, which equals JS `Math.round` for the non-negative values here.
fn fuzzy_max_distance(term: &str) -> usize {
    let len = term.chars().count() as f64;
    ((len * FUZZY_RATIO).round() as usize).min(MAX_FUZZY)
}

/// Tokenize then [`stem`] every token, dropping empties. Tokenization mirrors MiniSearch's default
/// `text.split(/[\n\r\p{Z}\p{P}]+/u)`.
fn stem_all(text: &str) -> Vec<String> {
    text.split(is_token_boundary)
        .filter(|t| !t.is_empty())
        .map(stem)
        .filter(|t| !t.is_empty())
        .collect()
}

/// MiniSearch's `SPACE_OR_PUNCTUATION = /[\n\r\p{Z}\p{P}]+/u`: CR/LF, any Unicode separator
/// (`\p{Z}` = Zs/Zl/Zp), or any Unicode punctuation (`\p{P}`). Uses the same real
/// `unicode-general-category` tables `normalize.rs` uses for `\p{P}`.
fn is_token_boundary(c: char) -> bool {
    use unicode_general_category::{get_general_category, GeneralCategory::*};
    if c == '\n' || c == '\r' {
        return true;
    }
    matches!(
        get_general_category(c),
        // \p{Z} — separators
        SpaceSeparator | LineSeparator | ParagraphSeparator
        // \p{P} — punctuation (Pc Pd Pe Pf Pi Po Ps)
            | ConnectorPunctuation
            | DashPunctuation
            | ClosePunctuation
            | FinalPunctuation
            | InitialPunctuation
            | OtherPunctuation
            | OpenPunctuation
    )
}

/// Verbatim port of the PWA's `stem()` (`lexicalSearch.js`): a deliberately small English
/// stemmer folding plurals + `-ing`/`-ed`, applied identically to indexed and query terms.
/// Lowercasing happens here (as in JS, `String(word).toLowerCase()`), via std `to_lowercase`
/// (Unicode 17.0, matching V8 `toLowerCase` — the same anchor `normalize_for_tag` relies on).
///
/// ponytail: lengths/slices are char-indexed (Unicode scalar values), which equals JS's UTF-16
/// `.length`/`.slice` for the entire BMP (all realistic tokens — Latin, accented, CJK). Only an
/// astral-plane codepoint inside a word long enough to hit a suffix rule would differ, and none
/// appear in the parity corpus.
pub fn stem(word: &str) -> String {
    let mut w = word.to_lowercase();
    let n = w.chars().count();
    if n <= 3 {
        return w;
    }

    // plurals (first match wins; the `!ss$` guard protects "boss"-style words)
    if w.ends_with("ies") && n > 4 {
        w = format!("{}y", take_chars(&w, n - 3));
    } else if w.ends_with("sses") {
        w = take_chars(&w, n - 2);
    } else if w.ends_with('s') && !w.ends_with("ss") {
        w = take_chars(&w, n - 1);
    }

    // -ing / -ed on the post-plural word, then undouble a doubled consonant
    let n2 = w.chars().count();
    if w.ends_with("ing") && n2 > 5 {
        w = undouble(&take_chars(&w, n2 - 3));
    } else if w.ends_with("ed") && n2 > 4 {
        w = undouble(&take_chars(&w, n2 - 2));
    }

    w
}

/// Drop a trailing doubled consonant, keeping the genuine doublets (`ll`/`ss`/`zz` and the other
/// non-`[bcdfghjkmnpqrtv]` letters): `"runn" → "run"`, `"stopp" → "stop"`, but `"miss"` stays.
/// Verbatim port of the PWA's `undouble()` (regex `/([bcdfghjkmnpqrtv])\1$/`).
fn undouble(w: &str) -> String {
    let chars: Vec<char> = w.chars().collect();
    let len = chars.len();
    if len >= 2 && chars[len - 1] == chars[len - 2] {
        let c = chars[len - 1];
        if matches!(
            c,
            'b' | 'c' | 'd' | 'f' | 'g' | 'h' | 'j' | 'k' | 'm' | 'n' | 'p' | 'q' | 'r' | 't' | 'v'
        ) {
            return take_chars(w, len - 1);
        }
    }
    w.to_string()
}

/// First `k` chars (Unicode scalar values) of `s` — the char-indexed analogue of JS `slice(0, k)`.
fn take_chars(s: &str, k: usize) -> String {
    s.chars().take(k).collect()
}

/// Distinct terms, order-preserving (small n — a linear `contains` is cheaper than a set here).
fn distinct(terms: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(terms.len());
    for t in terms {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Standard Levenshtein edit distance (insert/delete/substitute, no transposition) over Unicode
/// scalar values — the metric MiniSearch's fuzzy matcher uses.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, content: &str) -> SearchDoc {
        SearchDoc {
            kind: SearchDocKind::Note,
            ref_id: id.into(),
            title: String::new(),
            content: content.into(),
        }
    }
    fn idea(id: &str, name: &str, description: &str) -> SearchDoc {
        SearchDoc {
            kind: SearchDocKind::Idea,
            ref_id: id.into(),
            title: name.into(),
            content: description.into(),
        }
    }
    fn hit_ids(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.ref_id.as_str()).collect()
    }

    // ── stem() parity — the verbatim port (lexicalSearch.js oracle) ──────────

    #[test]
    fn stem_matches_the_js_oracle() {
        assert_eq!(stem("run"), "run"); // ≤3 chars: unchanged
        assert_eq!(stem("running"), "run"); // -ing → undouble nn→n
        assert_eq!(stem("stopped"), "stop"); // -ed → undouble pp→p
        assert_eq!(stem("connections"), "connection"); // -s (not -ss)
        assert_eq!(stem("parties"), "party"); // -ies → y
        assert_eq!(stem("classes"), "class"); // -sses → -ss
        assert_eq!(stem("boss"), "boss"); // -ss protected
        assert_eq!(stem("missed"), "miss"); // -ed → "miss"; ss NOT undoubled
        assert_eq!(stem("Antifragility"), "antifragility"); // lowercased, no suffix rule
        assert_eq!(stem("stoicism"), "stoicism"); // unaffected
        assert_eq!(stem("lies"), "lie"); // 4 chars: fails `>4` ies-guard, takes -s branch
    }

    #[test]
    fn tokenize_splits_on_unicode_punctuation_and_separators() {
        let terms = stem_all("Power & Justice — a, list.of\nthings");
        assert_eq!(terms, vec!["power", "justice", "a", "list", "of", "thing"]);
    }

    // ── SUR-754 parity fixtures (AC #5) — same verdicts as the PWA ───────────

    #[test]
    fn empty_or_whitespace_query_returns_nothing() {
        let docs = vec![note("n1", "The unexamined life is not worth living")];
        assert!(search(&docs, "", 10).is_empty());
        assert!(search(&docs, "   ", 10).is_empty());
    }

    #[test]
    fn no_match_returns_nothing() {
        let docs = vec![note("n1", "The unexamined life is not worth living")];
        assert!(search(&docs, "zzzznomatch", 10).is_empty());
    }

    #[test]
    fn basic_recall_hits_the_note() {
        let docs = vec![note("n1", "The unexamined life is not worth living")];
        let hits = search(&docs, "unexamined", 10);
        assert_eq!(hit_ids(&hits), vec!["n1"]);
        assert_eq!(hits[0].kind, SearchDocKind::Note);
    }

    #[test]
    fn stemming_meets_index_and_query_in_the_middle() {
        // indexed "running" ⇄ query "run"
        let docs = vec![note("n1", "a long running argument")];
        assert_eq!(hit_ids(&search(&docs, "run", 10)), vec!["n1"]);
        // indexed "run" ⇄ query "running"
        let docs = vec![note("n2", "they run every morning")];
        assert_eq!(hit_ids(&search(&docs, "running", 10)), vec!["n2"]);
        // indexed "connections" ⇄ query "connection"
        let docs = vec![note("n3", "surfacing hidden connections")];
        assert_eq!(hit_ids(&search(&docs, "connection", 10)), vec!["n3"]);
    }

    #[test]
    fn fuzzy_tolerates_a_typo() {
        // "Nietzche" (missing s) ⇄ "Nietzsche": Levenshtein 1 ≤ round(8·0.2)=2
        let docs = vec![note("n1", "Nietzsche on eternal recurrence")];
        assert_eq!(hit_ids(&search(&docs, "Nietzche", 10)), vec!["n1"]);
    }

    #[test]
    fn diacritics_case_is_fuzzy_tolerance_not_folding() {
        // AC #5 "diacritics": "cafe" ⇄ "café" is a 1-edit substitution ≤ round(4·0.2)=1.
        let docs = vec![note("n1", "an afternoon in a café society")];
        assert_eq!(hit_ids(&search(&docs, "cafe", 10)), vec!["n1"]);
    }

    #[test]
    fn prefix_matches_a_longer_term() {
        // "stoic" is a strict prefix of "stoicism" (neither is changed by stem()).
        let docs = vec![idea("i1", "Stoicism", "")];
        let hits = search(&docs, "stoic", 10);
        assert_eq!(hit_ids(&hits), vec!["i1"]);
        assert_eq!(hits[0].kind, SearchDocKind::Idea);
    }

    #[test]
    fn custom_ideas_index_name_and_description() {
        let docs = vec![idea(
            "i1",
            "Antifragility",
            "improvement by removing, not adding",
        )];
        assert_eq!(hit_ids(&search(&docs, "antifragility", 10)), vec!["i1"]); // name
        assert_eq!(hit_ids(&search(&docs, "removing", 10)), vec!["i1"]); // description
    }

    #[test]
    fn result_shape_and_multi_doc_recall() {
        let docs = vec![
            note("n1", "the rhizome has no beginning"),
            idea("i1", "Rhizome", "a non-hierarchical network"),
        ];
        let hits = search(&docs, "rhizome", 10);
        assert!(hits.len() >= 2, "both the note and the idea should surface");
        for h in &hits {
            assert!(!h.ref_id.is_empty());
            assert!(!h.snippet.is_empty());
            assert!(h.score > 0.0);
        }
    }

    #[test]
    fn limit_caps_results() {
        let docs: Vec<SearchDoc> = (0..5)
            .map(|i| note(&format!("n{i}"), "shared keyword"))
            .collect();
        assert_eq!(search(&docs, "keyword", 3).len(), 3);
    }
}
