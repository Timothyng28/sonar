//! Read a source file off disk and prepare the content for tantivy
//! indexing. Two concerns:
//!
//! 1. **Encoding.** We only handle UTF-8. Files with invalid UTF-8 are
//!    skipped — overwhelmingly they're binary blobs we missed in the
//!    extension sniff.
//!
//! 2. **Identifier splitting.** tantivy's default tokenizer treats
//!    `processOrderItem` as one term and indexes nothing useful for
//!    queries like `"order"`. We pre-process the text to split
//!    camelCase and PascalCase boundaries by injecting a space, so the
//!    same word also gets indexed as `process order item`.
//!    snake_case and kebab-case are already handled by tantivy's default
//!    tokenizer (it splits on non-alphanumeric).

use anyhow::Result;
use std::fs;
use std::path::Path;

/// Read file content + return a tantivy-friendly version with camelCase
/// boundaries pre-split. Original lines are also preserved so the
/// `content` field still shows readable code in snippets.
pub fn read_and_prepare(path: &Path) -> Result<String> {
    let raw = fs::read_to_string(path)?;
    Ok(expand_identifiers(&raw))
}

/// Insert spaces at camelCase / PascalCase boundaries while leaving the
/// original characters intact. Conservative — only splits on
/// `lowercase|digit → Uppercase` transitions. Sequences of capitals
/// (e.g. `HTTPRequest`) are split as `HTTP Request` by the
/// `Uppercase|Uppercase|lowercase` rule.
///
/// Examples:
///   processOrderItem  →  processOrderItem process Order Item
///   HTTPRequest       →  HTTPRequest HTTP Request
///   foo_bar           →  foo_bar          (tantivy splits on _)
///   ccai_2639         →  ccai_2639        (digits stay attached)
///
/// We append the expanded form rather than replacing so phrase queries
/// for the original (e.g. `"processOrderItem"`) still work too.
pub fn expand_identifiers(s: &str) -> String {
    let expanded = split_camel(s);
    if expanded == s {
        s.to_string()
    } else {
        // Indexing both forms means an exact-phrase query and a token
        // query both find the file. The duplication costs little in
        // index size because the inverted-index posting list dedups.
        format!("{s}\n{expanded}")
    }
}

fn split_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    let mut prev: Option<char> = None;
    let mut prev_prev: Option<char> = None;
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let next = chars.get(i + 1).copied();
        let should_split = match (prev, c, next) {
            // lower|digit → Upper
            (Some(p), c, _) if is_word(p) && p.is_lowercase() && c.is_uppercase() => true,
            (Some(p), c, _) if p.is_ascii_digit() && c.is_alphabetic() => true,
            (Some(p), c, _) if p.is_alphabetic() && c.is_ascii_digit() => true,
            // Upper|Upper|lower (e.g. HTTPRequest → HTTP Request between P and R)
            (Some(p), c, Some(n))
                if p.is_uppercase()
                    && c.is_uppercase()
                    && n.is_lowercase()
                    && prev_prev.map(|pp| pp.is_uppercase()).unwrap_or(true) =>
            {
                true
            }
            _ => false,
        };
        if should_split {
            out.push(' ');
        }
        out.push(c);
        prev_prev = prev;
        prev = Some(c);
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_camel_case() {
        assert_eq!(split_camel("processOrderItem"), "process Order Item");
    }

    #[test]
    fn splits_pascal_case() {
        assert_eq!(split_camel("FooBarBaz"), "Foo Bar Baz");
    }

    #[test]
    fn keeps_consecutive_capitals_grouped() {
        assert_eq!(split_camel("HTTPRequest"), "HTTP Request");
        assert_eq!(split_camel("parseHTTPHeader"), "parse HTTP Header");
    }

    #[test]
    fn splits_digit_letter_boundary() {
        assert_eq!(split_camel("ccai2639"), "ccai 2639");
        assert_eq!(split_camel("v2Api"), "v 2 Api");
    }

    #[test]
    fn leaves_snake_case_alone() {
        // tantivy default tokenizer splits on `_` for us.
        assert_eq!(split_camel("foo_bar_baz"), "foo_bar_baz");
    }

    #[test]
    fn expand_keeps_original() {
        let out = expand_identifiers("processOrder");
        assert!(out.contains("processOrder"));
        assert!(out.contains("process Order"));
    }

    #[test]
    fn expand_noop_when_no_camel() {
        let out = expand_identifiers("just plain words");
        assert_eq!(out, "just plain words");
    }
}
