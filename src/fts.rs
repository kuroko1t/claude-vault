//! The single tokenization rule for full-text search.
//!
//! SQLite's `unicode61` tokenizer treats a whole run of CJK characters as one
//! token, so substring searches never match. Scripts written without spaces
//! are therefore split into one token per character, both when text is indexed
//! ([`index_text`]) and when a query is built ([`match_query`]). A CJK word in
//! a query becomes an FTS5 phrase of single-character tokens, which is an exact
//! substring match. Other text is left alone, so Porter stemming still applies.

use std::borrow::Cow;

/// Characters from scripts that are written without spaces between words.
fn is_unspaced_script(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}' // Hiragana, Katakana
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
        | '\u{AC00}'..='\u{D7AF}' // Hangul syllables
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
    )
}

/// Text to store in the FTS index: each CJK character becomes its own token.
pub fn index_text(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_unspaced_script) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if is_unspaced_script(c) {
            out.push(' ');
            out.push(c);
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Build an FTS5 MATCH expression from a user query.
pub fn match_query(query: &str) -> String {
    let escaped = escape_fts_query(query);
    let mut out = String::with_capacity(escaped.len() * 2);
    let mut in_quote = false;
    let mut in_run = false; // inside an unquoted CJK run we opened a phrase for
    for c in escaped.chars() {
        let cjk = is_unspaced_script(c);
        if in_run && !cjk {
            out.push('"');
            in_run = false;
        }
        if c == '"' {
            in_quote = !in_quote;
            out.push(c);
        } else if cjk && in_quote {
            out.push(' ');
            out.push(c);
            out.push(' ');
        } else if cjk {
            if in_run {
                out.push(' ');
            } else {
                out.push('"');
                in_run = true;
            }
            out.push(c);
        } else {
            out.push(c);
        }
    }
    if in_run {
        out.push('"');
    }
    out
}

/// Escape a query string for safe use in FTS5 MATCH.
/// Wraps each token in double quotes to prevent FTS5 operator interpretation.
fn escape_fts_query(query: &str) -> String {
    // If the user already used explicit FTS5 syntax (AND, OR, NOT, quotes), pass through
    if query.contains('"')
        || query.contains(" AND ")
        || query.contains(" OR ")
        || query.contains(" NOT ")
    {
        return query.to_string();
    }
    // Otherwise, quote each whitespace-separated token
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_token_is_spaced_per_character() {
        assert_eq!(match_query("預算"), "\" 預  算 \"");
        assert_eq!(match_query("SRM預算"), "\"SRM 預  算 \"");
    }

    #[test]
    fn latin_query_is_unchanged() {
        assert_eq!(match_query("running fast"), "\"running\" \"fast\"");
    }

    #[test]
    fn explicit_syntax_wraps_unquoted_cjk_runs_as_phrases() {
        assert_eq!(match_query("預算 OR 月結"), "\"預 算\" OR \"月 結\"");
        assert_eq!(match_query("foo AND 預算"), "foo AND \"預 算\"");
    }

    #[test]
    fn explicit_quotes_are_spaced_not_double_wrapped() {
        assert_eq!(match_query("\"預算\" OR x"), "\" 預  算 \" OR x");
    }

    #[test]
    fn empty_query_stays_empty() {
        assert_eq!(match_query("  "), "");
    }

    #[test]
    fn index_text_borrows_when_no_cjk() {
        assert!(matches!(index_text("plain ascii text"), Cow::Borrowed(_)));
        assert!(matches!(index_text("héllo wörld"), Cow::Borrowed(_)));
    }

    #[test]
    fn index_text_spaces_each_cjk_character() {
        let t = index_text("SRM預算ok");
        assert_eq!(
            t.split_whitespace().collect::<Vec<_>>(),
            ["SRM", "預", "算", "ok"]
        );
        let t = index_text("設定を変更");
        assert_eq!(t.split_whitespace().count(), 5);
    }
}
