//! # sturdy-compact
//!
//! Context-window management by *AST-aware* compaction. When a source file is
//! too large to fit the budget, we don't truncate blindly — we parse it with
//! Tree-sitter and keep the structural skeleton (every function/method/type
//! signature, doc comments, imports) while eliding function *bodies*, which are
//! usually the bulk of the tokens and the least useful for high-level reasoning.
//!
//! Five languages are supported — **Rust, Python, JavaScript, TypeScript, Go** —
//! selected explicitly or by file extension. Each knows which node kinds are
//! "functions" and how to spell an elided body in its own syntax.

use std::path::Path;

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
    #[error("unsupported language for `.{0}` (supported: rs, py, js, ts, go)")]
    UnsupportedExtension(String),
}

impl From<CompactError> for HarnessError {
    fn from(e: CompactError) -> Self {
        HarnessError::backend("compact", e)
    }
}

pub type Result<T> = std::result::Result<T, CompactError>;

/// A source language the compactor understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
        }
    }

    /// Map a file extension (without the dot) to a language.
    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "go" => Language::Go,
            _ => return None,
        })
    }

    /// Detect a language from a path's extension.
    pub fn from_path(path: impl AsRef<Path>) -> Option<Self> {
        path.as_ref()
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// Node kinds whose `body` field we elide.
    fn function_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["function_item"],
            Language::Python => &["function_definition"],
            Language::JavaScript | Language::TypeScript => &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "function_expression",
            ],
            Language::Go => &["function_declaration", "method_declaration"],
        }
    }

    /// How to spell an elided body in this language's syntax.
    fn body_placeholder(self, lines: usize) -> String {
        match self {
            // Python bodies are indented suites, not brace blocks.
            Language::Python => format!("...  # {lines} lines elided"),
            _ => format!("{{ /* {lines} lines elided */ }}"),
        }
    }
}

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

/// AST-aware compactor bound to one language.
pub struct Compactor {
    parser: Parser,
    language: Language,
}

impl Compactor {
    /// Create a compactor for a given language.
    pub fn new(language: Language) -> Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&language.grammar())?;
        Ok(Compactor { parser, language })
    }

    /// Convenience for the Rust grammar.
    pub fn rust() -> Result<Self> {
        Self::new(Language::Rust)
    }

    /// Build a compactor by detecting the language from a path's extension.
    pub fn for_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let lang = Language::from_path(path).ok_or_else(|| {
            CompactError::UnsupportedExtension(
                path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_string(),
            )
        })?;
        Self::new(lang)
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// Produce a structural outline: every signature kept, every function body
    /// replaced with a one-line placeholder recording how much was elided.
    pub fn outline(&mut self, source: &str) -> Result<CompactResult> {
        let tree = self
            .parser
            .parse(source, None)
            .ok_or(CompactError::ParseFailed)?;
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        collect_body_elisions(tree.root_node(), source, self.language, &mut edits);
        edits.sort_by_key(|e| e.0);

        let mut out = String::with_capacity(source.len());
        let mut pos = 0;
        for (start, end, replacement) in &edits {
            if *start < pos {
                continue; // guard against any overlap
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
    /// compact it to an outline (best-effort — a file that is all signatures may
    /// still exceed the budget).
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
fn collect_body_elisions(
    node: Node,
    source: &str,
    lang: Language,
    edits: &mut Vec<(usize, usize, String)>,
) {
    let kinds = lang.function_kinds();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            if let Some(body) = child.child_by_field_name("body") {
                let span = &source[body.start_byte()..body.end_byte()];
                let lines = span.lines().count().max(1);
                let replacement = lang.body_placeholder(lines);
                // Only elide when it actually shrinks the source.
                if replacement.len() < span.len() {
                    edits.push((body.start_byte(), body.end_byte(), replacement));
                }
            }
            // Do not recurse into the function; its body is gone.
        } else {
            // Descend into impls, classes, modules, etc. to reach nested functions.
            collect_body_elisions(child, source, lang, edits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = r#"
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
    for i in a..b { acc += i * i; }
    acc
}
"#;

    #[test]
    fn estimate_is_monotonic() {
        assert!(estimate_tokens("short") < estimate_tokens("a considerably longer string here"));
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn rust_outline_keeps_signatures_and_elides_bodies() {
        let mut c = Compactor::rust().unwrap();
        let r = c.outline(RUST).unwrap();
        assert!(r.text.contains("pub struct Widget"));
        assert!(r.text.contains("pub fn new(id: u32) -> Self"));
        assert!(r.text.contains("pub fn top_level(a: i32, b: i32) -> i32"));
        assert!(r.text.contains("/// Build one."));
        assert!(!r.text.contains("compute_expensive_thing"));
        assert!(!r.text.contains("acc += i * i"));
        assert_eq!(r.elided_bodies, 2);
        assert!(r.compacted_tokens < r.original_tokens);
    }

    #[test]
    fn python_outline_keeps_defs_and_uses_ellipsis() {
        let src = "def compute(a, b):\n    total = 0\n    for i in range(a, b):\n        total += i * i\n    return total\n\nclass Widget:\n    def build(self, n):\n        secret = expensive(n)\n        return secret\n";
        let mut c = Compactor::new(Language::Python).unwrap();
        let r = c.outline(src).unwrap();
        assert!(r.text.contains("def compute(a, b):"));
        assert!(r.text.contains("def build(self, n):"));
        assert!(r.text.contains("class Widget:"));
        assert!(r.text.contains("..."));
        assert!(!r.text.contains("total += i * i"));
        assert!(!r.text.contains("expensive(n)"));
        assert_eq!(r.elided_bodies, 2);
    }

    #[test]
    fn javascript_outline_elides_function_and_method_bodies() {
        let src = "function add(a, b) {\n  const s = a + b;\n  return s;\n}\nclass C {\n  method(x) {\n    const y = heavyThing(x);\n    return y;\n  }\n}\n";
        let mut c = Compactor::new(Language::JavaScript).unwrap();
        let r = c.outline(src).unwrap();
        assert!(r.text.contains("function add(a, b)"));
        assert!(r.text.contains("method(x)"));
        assert!(!r.text.contains("heavyThing"));
        assert_eq!(r.elided_bodies, 2);
    }

    #[test]
    fn go_outline_elides_func_and_method_bodies() {
        let src = "package main\nfunc Add(a, b int) int {\n\ts := a + b\n\treturn s\n}\nfunc (w Widget) Build(n int) int {\n\tsecret := expensive(n)\n\treturn secret\n}\n";
        let mut c = Compactor::new(Language::Go).unwrap();
        let r = c.outline(src).unwrap();
        assert!(r.text.contains("func Add(a, b int) int"));
        assert!(r.text.contains("func (w Widget) Build(n int) int"));
        assert!(!r.text.contains("expensive(n)"));
        assert_eq!(r.elided_bodies, 2);
    }

    #[test]
    fn language_detection_by_extension() {
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("PY"), Some(Language::Python));
        assert_eq!(Language::from_extension("tsx"), None); // tsx not wired
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        assert_eq!(Language::from_extension("txt"), None);
        assert_eq!(Language::from_path("src/main.rs"), Some(Language::Rust));
    }

    #[test]
    fn for_path_rejects_unknown_extensions() {
        assert!(matches!(
            Compactor::for_path("data.txt"),
            Err(CompactError::UnsupportedExtension(_))
        ));
    }

    #[test]
    fn compaction_never_grows_the_source() {
        let mut c = Compactor::rust().unwrap();
        let tiny = "fn a() { 1 }\nfn b() -> i32 { 2 }\n";
        let r = c.outline(tiny).unwrap();
        assert!(r.compacted_tokens <= r.original_tokens);
        assert_eq!(r.elided_bodies, 0);
    }
}
