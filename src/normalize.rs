//! `normalizeForTag` — canonical normalization for the content-dedup fingerprint.
//! Mirrors `src/lib/text.js` exactly:
//!
//! ```text
//! NFKC → toLowerCase → collapse /\s+/ → strip /\p{Cc}/ → trim
//!      → strip trailing /[\p{P}]+/ → trim
//! ```
//!
//! ## Unicode-version reconciliation (SUR-716; anchor = V8/Node Unicode 17.0)
//!
//! - **NFKC** → `unicode-normalization` 0.1.25 = Unicode **17.0** ✓ (matches anchor).
//! - **toLowerCase** → std `str::to_lowercase` = Unicode **17.0**, full SpecialCasing
//!   incl. the Greek final-sigma rule, matching JS `String.prototype.toLowerCase`.
//! - **`\p{Cc}`** → std `char::is_control` = Unicode **17.0** ✓ (Cc is also extremely
//!   stable across versions).
//! - **`\p{P}` / `\p{Zs}`** → `unicode-general-category` 1.1.0 = Unicode **16.0**.
//!   No real-tables General_Category crate is at 17.0 yet (`regex-syntax` is also 16.0),
//!   so this is the single residual skew vs the anchor: a codepoint whose P/Zs
//!   membership *changes* in 17.0 would diverge. None of the parity vectors hit that;
//!   the B6 differential fuzz characterizes the residue. Using real tables (not the
//!   spike's hand-coded ranges) closes the crypto-reviewer "real tables" condition.

use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

pub fn normalize_for_tag(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let nfkc: String = text.nfkc().collect();
    let lower = nfkc.to_lowercase();
    let collapsed = collapse_whitespace(&lower);
    // Strip /\p{Cc}/ AFTER whitespace collapse, so tab/newline (which are both \s and
    // Cc) become a real space boundary first and only the non-whitespace controls
    // (NUL, BEL, …) are removed — matching the JS ordering.
    let no_ctrl: String = collapsed.chars().filter(|c| !c.is_control()).collect();
    let trimmed = no_ctrl.trim();
    let stripped = trimmed.trim_end_matches(is_punctuation);
    stripped.trim().to_string()
}

/// `/\s+/g` → single U+0020, matching ECMAScript `\s` EXACTLY — not Rust
/// `char::is_whitespace`, which differs at U+0085 (NEL, whitespace in Rust but not ES)
/// and U+FEFF (ES whitespace but not Rust). ES `\s` = Space_Separator (Zs) ∪
/// {TAB, LF, VT, FF, CR, SP, NBSP, ZWNBSP/FEFF, LS, PS}.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if is_ecmascript_whitespace(c) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

fn is_ecmascript_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{FEFF}'
            | '\u{2028}'
            | '\u{2029}'
    ) || get_general_category(c) == GeneralCategory::SpaceSeparator
}

/// `\p{P}` = Unicode General_Category Punctuation (Pc, Pd, Pe, Pf, Pi, Po, Ps).
fn is_punctuation(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::OtherPunctuation
            | GeneralCategory::OpenPunctuation
    )
}
