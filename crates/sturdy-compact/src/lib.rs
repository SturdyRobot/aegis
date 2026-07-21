//! # sturdy-compact
//!
//! Context-window management by *AST-aware* compaction. When a source file is
//! too large to fit the budget, we don't truncate blindly — we parse it with
//! Tree-sitter and keep the structural skeleton (every function/struct/impl
//! signature, doc comments, `use`s) while eliding function *bodies*, which are
//! usually the bulk of the tokens and the least useful for high-level reasoning.
//!
//! The parser and estimator are pure Rust over `&str`, so the compaction logic
//! is portable (the only native piece is the Tree-sitter grammar itself).

use serde::Serialize;
use thiserror::Error;
use tree_sitter::{Node, Parser};

use sturdy_core::HarnessError;

#[derive(Debug, Error)]
pub enum CompactError {
    #[error("failed to load Tree-sitter grammar: {0}")]
    Language(#[from] tree_sitter::LanguageError),
    #[error("parser produced no tree")]
    ParseFailed,
}

impl From<CompactError> for HarnessError {
    fn from(e: CompactError) -> Self {
        HarnessError::backend("compact", e)
    }
}

pub type Result<T> = std::result::Result<T, CompactError>;

/// Approximate token count.
///
/// A real BPE tokenizer is model-specific; for budgeting we use the widely-used
/// heuristic of ~3.7 characters per token, which tracks code closely enough to
/// drive compaction decisions.
pub fn estimate_tokens(text: &str) -> usize {
    const CHARS_PER_TOKEN: f64 = 3.7;
    (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize
}

/// The outcome of a compaction pass.
#[derive(Debug, Clone, Serialize)]
pub struct CompactResult {
    pub text: String,
    pub original_tokens: usize,
    pub compacted_tokens: usize,
    /// How many function bodies were elided.
    pub elided_bodies: usize,
}

impl CompactResult {
    /// Fraction of tokens removed (0.0 = unchanged).
    pub fn savings(&self) -> f64 {
        if self.original_tokens == 0 {
            return 0.0;
        }
        1.0 - (self.compacted_tokens as f64 / self.original_tokens as f64)
    }
}

/// AST-aware compactor for Rust source.
pub struct Compactor {
    parser: Parser,
}

impl Compactor {
    /// Create a compactor bound to the Rust grammar.
    pub fn rust() -> Result<Self> {
        let mut parser = Parser::new();
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language)?;
        Ok(Compactor { parser })
    }

    /// Produce a structural outline: every signature kept, every function body
    /// replaced with a one-line placeholder recording how much was elided.
    pub fn outline(&mut self, source: &str) -> Result<CompactResult> {
        let tree = self
            .parser
            .parse(source, None)
            .ok_or(CompactError::ParseFailed)?;
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        collect_body_elisions(tree.root_node(), source, &mut edits);
        edits.sort_by_key(|e| e.0);

        let mut out = String::with_capacity(source.len());
        let mut pos = 0;
        for (start, end, replacement) in &edits {
            // Guard against any (shouldn't happen) overlap from nested matches.
            if *start < pos {
                continue;
            }
            out.push_str(&source[pos..*start]);
            out.push_str(replacement);
            pos = *end;
        }
        out.push_str(&source[pos..]);

        Ok(CompactResult {
            original_tokens: estimate_tokens(source),
            compacted_tokens: estimate_tokens(&out),
            elided_bodies: edits.len(),
            text: out,
        })
    }

    /// Return `source` untouched if it already fits `max_tokens`; otherwise
    /// compact it to an outline. The outline is best-effort — a file that is all
    /// signatures may still exceed the budget, and the caller can decide what to
    /// do with the (smaller) result.
    pub fn compact_to_budget(&mut self, source: &str, max_tokens: usize) -> Result<CompactResult> {
        let original = estimate_tokens(source);
        if original <= max_tokens {
            return Ok(CompactResult {
                text: source.to_string(),
                original_tokens: original,
                compacted_tokens: original,
                elided_bodies: 0,
            });
        }
        self.outline(source)
    }
}

/// Walk the tree collecting `(start, end, replacement)` for each function body,
/// without descending into a body once found (its contents are elided anyway).
fn collect_body_elisions(node: Node, source: &str, edits: &mut Vec<(usize, usize, String)>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_item" {
            if let Some(body) = child.child_by_field_name("body") {
                let span = &source[body.start_byte()..body.end_byte()];
                let lines = span.lines().count().max(1);
                edits.push((
                    body.start_byte(),
                    body.end_byte(),
                    format!("{{ /* {lines} lines elided */ }}"),
                ));
            }
            // Do not recurse into the function; its body is gone.
        } else {
            // Descend into impls, modules, etc. to reach nested functions.
            collect_body_elisions(child, source, edits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
/// A widget.
pub struct Widget { pub id: u32 }

impl Widget {
    /// Build one.
    pub fn new(id: u32) -> Self {
        let secret = compute_expensive_thing(id);
        Widget { id: secret }
    }
}

pub fn top_level(a: i32, b: i32) -> i32 {
    let mut acc = 0;
    for i in a..b {
        acc += i * i;
    }
    acc
}
"#;

    #[test]
    fn estimate_is_monotonic() {
        assert!(estimate_tokens("short") < estimate_tokens("a considerably longer string here"));
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn outline_keeps_signatures_and_elides_bodies() {
        let mut c = Compactor::rust().unwrap();
        let r = c.outline(SAMPLE).unwrap();

        // Signatures survive.
        assert!(r.text.contains("pub struct Widget"));
        assert!(r.text.contains("pub fn new(id: u32) -> Self"));
        assert!(r.text.contains("pub fn top_level(a: i32, b: i32) -> i32"));
        // Doc comments survive.
        assert!(r.text.contains("/// Build one."));
        // Bodies are gone.
        assert!(!r.text.contains("compute_expensive_thing"));
        assert!(!r.text.contains("acc += i * i"));
        assert!(r.text.contains("elided"));
        // Two function bodies elided (new + top_level).
        assert_eq!(r.elided_bodies, 2);
        // And it actually saved tokens.
        assert!(r.compacted_tokens < r.original_tokens);
    }

    #[test]
    fn under_budget_is_untouched() {
        let mut c = Compactor::rust().unwrap();
        let r = c.compact_to_budget(SAMPLE, 100_000).unwrap();
        assert_eq!(r.text, SAMPLE);
        assert_eq!(r.elided_bodies, 0);
        assert_eq!(r.savings(), 0.0);
    }

    #[test]
    fn over_budget_compacts() {
        let mut c = Compactor::rust().unwrap();
        // Force compaction with a tiny budget.
        let r = c.compact_to_budget(SAMPLE, 5).unwrap();
        assert!(r.elided_bodies > 0);
        assert!(r.savings() > 0.0);
    }
}
