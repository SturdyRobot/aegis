//! # kedge_rt — Python bindings for Kedge
//!
//! `pip install kedge-rt` and use Kedge's Rust engine directly from Python — the
//! AST compactor, content hashing, tool safety classification, the policy matcher,
//! and the forensic audit report — without rewriting anything into Rust.
//!
//! The value here is **distribution, not raw speed**: a 1.5 s LLM call dwarfs any
//! FFI overhead. It puts battle-tested Rust primitives into every FastAPI/Python
//! codebase. This first cut exposes the synchronous, high-value surface; async
//! agent execution + subagent supervision are a follow-on.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use sha2::{Digest, Sha256};
use kedge_compact::{Compactor, Language};

fn value_err<E: std::fmt::Display>(e: E) -> PyErr {
    PyValueError::new_err(e.to_string())
}

fn parse_lang(lang: &str) -> PyResult<Language> {
    Ok(match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Language::Rust,
        "python" | "py" => Language::Python,
        "javascript" | "js" => Language::JavaScript,
        "typescript" | "ts" => Language::TypeScript,
        "go" => Language::Go,
        other => {
            return Err(PyValueError::new_err(format!(
                "unsupported language `{other}` (rust|python|javascript|typescript|go)"
            )))
        }
    })
}

/// `sha256` content hash of `source` — the deterministic cache key Kedge uses.
#[pyfunction]
fn content_hash(source: &str) -> String {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    format!("{:x}", h.finalize())
}

/// AST-aware compaction: keep the code skeleton, elide function bodies. Returns a
/// dict with `text`, `original_tokens`, `compacted_tokens`, `elided_bodies`,
/// `savings` (fraction of tokens removed).
#[pyfunction]
#[pyo3(signature = (source, lang="rust"))]
fn compact<'py>(py: Python<'py>, source: &str, lang: &str) -> PyResult<Bound<'py, PyDict>> {
    let mut compactor = Compactor::new(parse_lang(lang)?).map_err(value_err)?;
    let result = compactor.outline(source).map_err(value_err)?;
    let savings = result.savings(); // borrow before we move `text` out
    let d = PyDict::new(py);
    d.set_item("original_tokens", result.original_tokens)?;
    d.set_item("compacted_tokens", result.compacted_tokens)?;
    d.set_item("elided_bodies", result.elided_bodies)?;
    d.set_item("savings", savings)?;
    d.set_item("text", result.text)?;
    Ok(d)
}

/// Classify a tool by name. Returns `(safety, risk)` where safety is
/// `"read_only"` or `"mutating"` and risk is `None` / `"medium"` / `"high"`.
/// Fail-safe: anything not clearly read-only is `mutating`.
#[pyfunction]
fn classify_tool(name: &str) -> (String, Option<String>) {
    match kedge_audit::classify(name) {
        kedge_audit::ToolSafety::ReadOnly => ("read_only".to_string(), None),
        kedge_audit::ToolSafety::Mutating { risk } => {
            ("mutating".to_string(), Some(risk.as_str().to_string()))
        }
    }
}

/// A parsed Kedge policy (blocked tools, PII redaction, budgets).
#[pyclass]
struct Policy {
    inner: kedge_policy::Policy,
}

#[pymethods]
impl Policy {
    /// Parse a policy from `kedge-policy.toml` text.
    #[staticmethod]
    fn from_toml(text: &str) -> PyResult<Self> {
        Ok(Policy {
            inner: kedge_policy::Policy::from_toml_str(text).map_err(value_err)?,
        })
    }

    /// Whether `tool` is permitted (not on the blocked list).
    fn allows_tool(&self, tool: &str) -> bool {
        self.inner.allows_tool(tool)
    }

    /// Redact configured PII patterns from `text`.
    fn redact(&self, text: &str) -> String {
        self.inner.redact(text)
    }
}

/// Shadow-Guard forensic audit of a ledger, returned as a JSON string
/// (`json.loads` it). `price_per_1k` / `runs_per_day` are your inputs for the
/// optional cost projection.
#[pyfunction]
#[pyo3(signature = (db_path, price_per_1k=None, runs_per_day=None))]
fn audit_report(
    db_path: &str,
    price_per_1k: Option<f64>,
    runs_per_day: Option<u64>,
) -> PyResult<String> {
    let report = kedge_audit::AuditReport::from_ledger(db_path, price_per_1k, runs_per_day)
        .map_err(value_err)?;
    Ok(report.to_json())
}

/// The `kedge_rt` Python module.
#[pymodule]
fn kedge_rt(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(content_hash, m)?)?;
    m.add_function(wrap_pyfunction!(compact, m)?)?;
    m.add_function(wrap_pyfunction!(classify_tool, m)?)?;
    m.add_function(wrap_pyfunction!(audit_report, m)?)?;
    m.add_class::<Policy>()?;
    Ok(())
}
