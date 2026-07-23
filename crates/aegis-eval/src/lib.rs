//! # aegis-eval
//!
//! An event-sourced regression harness for agents. Every Aegis run is journaled
//! to a ledger; this crate turns two ledgers — a **baseline** and a **candidate**
//! (e.g. the same prompt re-run under a new system prompt or model) — into a
//! pass/fail regression report suitable for CI.
//!
//! The comparison itself is deterministic and needs no LLM: it profiles each run
//! (step count, tool-call sequence, tokens, final answer) and scores a set of
//! metrics with thresholds. Producing the candidate ledger (an `aegis run`) is a
//! separate step, so this stays a pure, testable comparison engine.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sturdy_core::{Action, TaskId, Trajectory};
use sturdy_ledger::Ledger;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("ledger error: {0}")]
    Ledger(#[from] sturdy_ledger::LedgerError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ledger `{0}` contains no runs")]
    NoRuns(PathBuf),
    #[error("malformed task id in ledger")]
    BadTaskId,
}

// ── suite schema ──

/// A metric to score a candidate run against the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// Same number of steps.
    StepCountParity,
    /// Same tools called in the same order.
    ToolCallEquivalence,
    /// Token growth within the allowed threshold.
    TokenDeltaThreshold,
    /// Final-answer similarity above the drift floor.
    OutputDrift,
}

/// Pass/fail thresholds. Defaults are deliberately lenient.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thresholds {
    /// Max allowed fractional token growth, e.g. `0.15` = +15%.
    #[serde(default = "default_token_delta_max")]
    pub token_delta_max: f64,
    /// Min lexical similarity of final answers, 0.0–1.0.
    #[serde(default = "default_drift_min")]
    pub drift_min: f64,
}

fn default_token_delta_max() -> f64 {
    0.15
}
fn default_drift_min() -> f64 {
    0.9
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            token_delta_max: default_token_delta_max(),
            drift_min: default_drift_min(),
        }
    }
}

/// A parsed `eval_suite.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSuite {
    pub suite_name: String,
    pub baseline_ledger: PathBuf,
    pub metrics: Vec<MetricKind>,
    #[serde(default)]
    pub thresholds: Thresholds,
}

impl EvalSuite {
    pub fn from_json_file(path: impl AsRef<Path>) -> Result<Self, EvalError> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }
}

// ── run profile (the comparable shape of a recorded run) ──

/// The features of a run that regression metrics compare.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunProfile {
    pub steps: u32,
    pub tool_sequence: Vec<String>,
    pub total_tokens: u64,
    pub final_answer: String,
}

impl RunProfile {
    /// Derive a profile from a replayed trajectory.
    pub fn from_trajectory(traj: &Trajectory) -> Self {
        let mut p = RunProfile {
            steps: traj.steps.len() as u32,
            ..Default::default()
        };
        for step in &traj.steps {
            p.total_tokens += step.tokens;
            match &step.action {
                Action::Tool(call) => p.tool_sequence.push(call.name.clone()),
                Action::Finish { answer } => p.final_answer = answer.clone(),
            }
        }
        p
    }

    /// Load the first run recorded in a ledger file.
    pub fn load_first(path: impl AsRef<Path>) -> Result<Self, EvalError> {
        let path = path.as_ref();
        let ledger = Ledger::open(path)?;
        let runs = ledger.list_runs()?;
        let first = runs
            .first()
            .ok_or_else(|| EvalError::NoRuns(path.to_path_buf()))?;
        let task_id = TaskId(Uuid::parse_str(&first.task_id).map_err(|_| EvalError::BadTaskId)?);
        let mut profile = RunProfile::from_trajectory(&ledger.replay(task_id)?);
        if profile.final_answer.is_empty() {
            if let Ok(detail) = ledger.run_detail(task_id) {
                profile.final_answer = detail.answer.unwrap_or_default();
            }
        }
        Ok(profile)
    }
}

// ── metrics + report ──

/// One metric's outcome.
#[derive(Debug, Clone, Serialize)]
pub struct MetricResult {
    pub metric: String,
    pub passed: bool,
    pub detail: String,
}

/// The full regression report for one candidate vs. one baseline.
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub suite_name: String,
    pub passed: bool,
    pub results: Vec<MetricResult>,
}

/// Lexical (token-Jaccard) similarity of two answers, 0.0–1.0. A deterministic
/// stand-in for semantic drift — honest about being lexical, not embedding-based.
fn lexical_similarity(a: &str, b: &str) -> f64 {
    let toks = |s: &str| -> std::collections::BTreeSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .collect()
    };
    let (sa, sb) = (toks(a), toks(b));
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 {
        1.0
    } else {
        inter / union
    }
}

fn score_metric(
    kind: MetricKind,
    base: &RunProfile,
    cand: &RunProfile,
    th: &Thresholds,
) -> MetricResult {
    match kind {
        MetricKind::StepCountParity => MetricResult {
            metric: "step_count_parity".into(),
            passed: base.steps == cand.steps,
            detail: format!("baseline {} vs candidate {} steps", base.steps, cand.steps),
        },
        MetricKind::ToolCallEquivalence => MetricResult {
            metric: "tool_call_equivalence".into(),
            passed: base.tool_sequence == cand.tool_sequence,
            detail: format!(
                "baseline [{}] vs candidate [{}]",
                base.tool_sequence.join(", "),
                cand.tool_sequence.join(", ")
            ),
        },
        MetricKind::TokenDeltaThreshold => {
            let delta = if base.total_tokens == 0 {
                if cand.total_tokens == 0 {
                    0.0
                } else {
                    1.0
                }
            } else {
                (cand.total_tokens as f64 - base.total_tokens as f64) / base.total_tokens as f64
            };
            MetricResult {
                metric: "token_delta_threshold".into(),
                passed: delta <= th.token_delta_max,
                detail: format!(
                    "{} → {} tokens ({:+.1}%, max {:+.1}%)",
                    base.total_tokens,
                    cand.total_tokens,
                    delta * 100.0,
                    th.token_delta_max * 100.0
                ),
            }
        }
        MetricKind::OutputDrift => {
            let score = lexical_similarity(&base.final_answer, &cand.final_answer);
            MetricResult {
                metric: "output_drift".into(),
                passed: score >= th.drift_min,
                detail: format!("lexical similarity {score:.2} (min {:.2})", th.drift_min),
            }
        }
    }
}

/// Score every metric in `suite` for `candidate` vs `baseline`.
pub fn evaluate(suite: &EvalSuite, baseline: &RunProfile, candidate: &RunProfile) -> EvalReport {
    let results: Vec<MetricResult> = suite
        .metrics
        .iter()
        .map(|&m| score_metric(m, baseline, candidate, &suite.thresholds))
        .collect();
    EvalReport {
        suite_name: suite.suite_name.clone(),
        passed: results.iter().all(|r| r.passed),
        results,
    }
}

impl EvalReport {
    /// CI exit code: `0` all-pass, `1` any regression.
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.passed)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// A JUnit-compatible XML report for CI (GitHub Actions, etc.).
    pub fn to_junit(&self) -> String {
        let failures = self.results.iter().filter(|r| !r.passed).count();
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        xml.push_str(&format!(
            "<testsuite name=\"{}\" tests=\"{}\" failures=\"{}\">\n",
            xml_escape(&self.suite_name),
            self.results.len(),
            failures
        ));
        for r in &self.results {
            xml.push_str(&format!(
                "  <testcase name=\"{}\" classname=\"aegis-eval\">",
                xml_escape(&r.metric)
            ));
            if r.passed {
                xml.push_str("</testcase>\n");
            } else {
                xml.push_str(&format!(
                    "\n    <failure message=\"{}\"/>\n  </testcase>\n",
                    xml_escape(&r.detail)
                ));
            }
        }
        xml.push_str("</testsuite>\n");
        xml
    }

    /// A human-readable report.
    pub fn to_pretty(&self) -> String {
        let mut s = format!(
            "eval suite: {}  →  {}\n",
            self.suite_name,
            if self.passed { "PASS" } else { "FAIL" }
        );
        for r in &self.results {
            s.push_str(&format!(
                "  [{}] {:<24} {}\n",
                if r.passed { "ok" } else { "XX" },
                r.metric,
                r.detail
            ));
        }
        s
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Output format for the `aegis eval` CLI.
#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Json,
    Junit,
    Pretty,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "json" => Ok(OutputFormat::Json),
            "junit" => Ok(OutputFormat::Junit),
            "pretty" => Ok(OutputFormat::Pretty),
            other => Err(format!("unknown output format `{other}`")),
        }
    }
}

/// The `aegis eval` entry point: load the suite + baseline, compare against the
/// candidate ledger, print the rendered report, and return the CI exit code.
pub fn run_eval(
    suite_path: impl AsRef<Path>,
    candidate_ledger: impl AsRef<Path>,
    format: OutputFormat,
) -> Result<i32, EvalError> {
    let suite = EvalSuite::from_json_file(&suite_path)?;
    let baseline = RunProfile::load_first(&suite.baseline_ledger)?;
    let candidate = RunProfile::load_first(&candidate_ledger)?;
    let report = evaluate(&suite, &baseline, &candidate);
    let rendered = match format {
        OutputFormat::Json => report.to_json(),
        OutputFormat::Junit => report.to_junit(),
        OutputFormat::Pretty => report.to_pretty(),
    };
    println!("{rendered}");
    Ok(report.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sturdy_core::{Action, Observation, Step, Task, Thought, ToolCall};
    use sturdy_ledger::Ledger;

    fn suite(metrics: Vec<MetricKind>) -> EvalSuite {
        EvalSuite {
            suite_name: "core_agent_evals".into(),
            baseline_ledger: PathBuf::new(),
            metrics,
            thresholds: Thresholds::default(),
        }
    }

    fn profile(steps: u32, tools: &[&str], tokens: u64, answer: &str) -> RunProfile {
        RunProfile {
            steps,
            tool_sequence: tools.iter().map(|s| s.to_string()).collect(),
            total_tokens: tokens,
            final_answer: answer.into(),
        }
    }

    #[test]
    fn identical_runs_pass_every_metric() {
        let s = suite(vec![
            MetricKind::StepCountParity,
            MetricKind::ToolCallEquivalence,
            MetricKind::TokenDeltaThreshold,
            MetricKind::OutputDrift,
        ]);
        let base = profile(3, &["shell", "shell"], 100, "the answer is 42");
        let report = evaluate(&s, &base, &base.clone());
        assert!(report.passed, "{:?}", report.results);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn token_inflation_and_tool_drift_fail() {
        let s = suite(vec![
            MetricKind::TokenDeltaThreshold,
            MetricKind::ToolCallEquivalence,
        ]);
        let base = profile(2, &["search"], 100, "x");
        let cand = profile(2, &["search", "shell"], 130, "x"); // +30% tokens, extra tool
        let report = evaluate(&s, &base, &cand);
        assert!(!report.passed);
        assert_eq!(report.exit_code(), 1);
        assert!(report.results.iter().all(|r| !r.passed));
    }

    #[test]
    fn junit_renders_failure_tags() {
        let s = suite(vec![MetricKind::StepCountParity]);
        let base = profile(2, &[], 10, "");
        let cand = profile(5, &[], 10, "");
        let xml = evaluate(&s, &base, &cand).to_junit();
        assert!(xml.contains("<testsuite"));
        assert!(xml.contains("failures=\"1\""));
        assert!(xml.contains("<failure message="));
    }

    #[test]
    fn suite_parses_from_json() {
        let json = r#"{
            "suite_name": "core_agent_evals",
            "baseline_ledger": "./baselines/run_01234.sqlite",
            "metrics": ["step_count_parity", "tool_call_equivalence", "token_delta_threshold"]
        }"#;
        let s: EvalSuite = serde_json::from_str(json).unwrap();
        assert_eq!(s.metrics.len(), 3);
        assert_eq!(s.thresholds.token_delta_max, 0.15); // default applied
    }

    /// End-to-end over a real SQLite baseline: write a run, read it back into a
    /// profile, and evaluate a token-inflated candidate against it.
    #[test]
    fn evaluates_from_a_real_sqlite_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.sqlite");

        // Build a baseline run: 1 tool step + 1 finish.
        let ledger = Ledger::open(&path).unwrap();
        let task = Task::new("compute the answer");
        ledger.begin_run(&task).unwrap();
        ledger
            .record_step(
                task.id,
                &Step {
                    index: 0,
                    thought: Thought("use the tool".into()),
                    action: Action::Tool(ToolCall::new("calc", serde_json::json!({}))),
                    observation: Some(Observation::ok("42")),
                    tokens: 40,
                    elapsed_ms: 1,
                },
            )
            .unwrap();
        ledger
            .record_step(
                task.id,
                &Step {
                    index: 1,
                    thought: Thought("done".into()),
                    action: Action::Finish {
                        answer: "the answer is 42".into(),
                    },
                    observation: None,
                    tokens: 10,
                    elapsed_ms: 1,
                },
            )
            .unwrap();
        ledger
            .finalize(
                task.id,
                &sturdy_core::Outcome::Finished {
                    answer: "the answer is 42".into(),
                },
            )
            .unwrap();
        drop(ledger);

        let base = RunProfile::load_first(&path).unwrap();
        assert_eq!(base.steps, 2);
        assert_eq!(base.tool_sequence, vec!["calc"]);
        assert_eq!(base.total_tokens, 50);
        assert_eq!(base.final_answer, "the answer is 42");

        // A candidate that used 40% more tokens should trip the delta threshold.
        let mut cand = base.clone();
        cand.total_tokens = 70;
        let s = suite(vec![
            MetricKind::StepCountParity,
            MetricKind::TokenDeltaThreshold,
        ]);
        let report = evaluate(&s, &base, &cand);
        assert!(!report.passed);
        let delta = report
            .results
            .iter()
            .find(|r| r.metric == "token_delta_threshold")
            .unwrap();
        assert!(!delta.passed);
        // step parity still holds.
        assert!(
            report
                .results
                .iter()
                .find(|r| r.metric == "step_count_parity")
                .unwrap()
                .passed
        );
    }
}
