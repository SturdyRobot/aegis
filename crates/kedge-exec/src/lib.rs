//! # kedge-exec
//!
//! Isolated subprocess execution for verification loops. Two guarantees set it
//! apart from a bare `tokio::process::Command`:
//!
//! * **Process-group isolation.** Each child leads its own process group. When a
//!   timeout fires we signal the *whole group* (`killpg`), so a build that forks
//!   `rustc`/`cc`/linker children can never leak zombies past the deadline.
//! * **Compiler interception.** [`verify_rust`] runs `cargo build` with JSON
//!   diagnostics and parses them into structured [`Diagnostic`]s, turning a
//!   compile into a machine-checkable pass/fail the agent can react to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;

use kedge_core::HarnessError;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("i/o error while running `{program}`: {source}")]
    Io {
        program: String,
        source: std::io::Error,
    },
    #[error("output reader task panicked")]
    ReaderPanicked,
}

impl From<ExecError> for HarnessError {
    fn from(e: ExecError) -> Self {
        HarnessError::backend("exec", e)
    }
}

pub type Result<T> = std::result::Result<T, ExecError>;

/// A command to run under the isolated runner.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        CommandSpec {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            timeout: Duration::from_secs(120),
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }
}

/// The captured result of a finished (or timed-out) subprocess.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// Exit code, or `None` if killed (signal / timeout).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// True if the deadline fired and the process group was killed.
    pub timed_out: bool,
}

impl ProcessOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }
}

/// Kill an entire process group by its leader pid (which, because we spawn with
/// `process_group(0)`, equals the group id).
#[cfg(unix)]
fn kill_group(pid: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

async fn drain(mut r: impl AsyncReadExt + Unpin) -> std::io::Result<String> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Await an output-reader task with a hard upper bound. The caller has already
/// closed the pipes (killed the group), so this resolves at once in practice;
/// the timeout only exists so a pathological reader can never hang the runner.
async fn join_reader(
    program: &str,
    task: tokio::task::JoinHandle<std::io::Result<String>>,
) -> Result<String> {
    match tokio::time::timeout(Duration::from_secs(10), task).await {
        Ok(Ok(Ok(s))) => Ok(s),
        Ok(Ok(Err(source))) => Err(ExecError::Io {
            program: program.to_string(),
            source,
        }),
        Ok(Err(_join)) => Err(ExecError::ReaderPanicked),
        Err(_timeout) => Ok(String::new()),
    }
}

/// A short, stable digest of a command's arguments — recorded on traces so a
/// tool invocation can be correlated without logging (possibly sensitive) argv.
fn args_hash(args: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    args.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Run `spec` to completion or until its timeout, capturing stdout/stderr. On
/// timeout the whole process group is killed and `timed_out` is set.
#[tracing::instrument(
    name = "tool.exec",
    skip_all,
    fields(
        tool_name = %spec.program,
        args_hash = %args_hash(&spec.args),
        timeout_ms = spec.timeout.as_millis() as u64,
        exit_code = tracing::field::Empty,
        timed_out = tracing::field::Empty,
    )
)]
pub async fn run(spec: &CommandSpec) -> Result<ProcessOutput> {
    let mut std_cmd = std::process::Command::new(&spec.program);
    std_cmd.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        std_cmd.current_dir(cwd);
    }
    for (k, v) in &spec.env {
        std_cmd.env(k, v);
    }
    std_cmd.stdin(Stdio::null());
    std_cmd.stdout(Stdio::piped());
    std_cmd.stderr(Stdio::piped());

    // Put the child in a fresh process group so a timeout can reap its whole
    // subtree. (`process_group` is stable std since 1.64.)
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        std_cmd.process_group(0);
    }

    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|source| ExecError::Spawn {
        program: spec.program.clone(),
        source,
    })?;
    let pid = child.id();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_task = tokio::spawn(async move {
        match stdout {
            Some(s) => drain(s).await,
            None => Ok(String::new()),
        }
    });
    let err_task = tokio::spawn(async move {
        match stderr {
            Some(s) => drain(s).await,
            None => Ok(String::new()),
        }
    });

    let mut code = None;
    let timed_out = match tokio::time::timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) => {
            code = status.code();
            false
        }
        Ok(Err(source)) => {
            return Err(ExecError::Io {
                program: spec.program.clone(),
                source,
            })
        }
        Err(_) => true,
    };

    // Whether it finished cleanly or timed out, tear down the whole subtree so no
    // forked/backgrounded child is left running or holding the output pipes open.
    // `killpg` reaps the group on Unix; `start_kill` covers the leader everywhere.
    // (This is why a `sh -c '... & echo'` can't hang the reader below.)
    if let Some(p) = pid {
        kill_group(p);
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    // Drain with a hard safety bound — the pipes are closed by now, so this
    // returns immediately; the timeout is a last-resort guard against ever hanging.
    let stdout = join_reader(&spec.program, out_task).await?;
    let stderr = join_reader(&spec.program, err_task).await?;

    let span = tracing::Span::current();
    span.record("exit_code", code.unwrap_or(-1));
    span.record("timed_out", timed_out);

    Ok(ProcessOutput {
        code,
        stdout,
        stderr,
        timed_out,
    })
}

// ── compiler interception ──

/// One structured compiler diagnostic, distilled from `cargo`'s JSON stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: String,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

/// Parse a `cargo build --message-format=json` stream into diagnostics.
///
/// Cargo emits one JSON object per line; we keep only `compiler-message`
/// records and pull the primary span's location.
pub fn parse_cargo_json(stream: &str) -> Vec<Diagnostic> {
    stream
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v.get("reason").and_then(|r| r.as_str()) == Some("compiler-message"))
        .filter_map(|v| {
            let msg = v.get("message")?;
            let level = msg.get("level")?.as_str()?.to_string();
            // Cargo repeats the top-level summary as level "error"/"warning"
            // with an empty span list — those carry no location, which is fine.
            let text = msg.get("message")?.as_str()?.to_string();
            let span = msg.get("spans").and_then(|s| s.as_array()).and_then(|arr| {
                arr.iter()
                    .find(|s| s.get("is_primary") == Some(&serde_json::Value::Bool(true)))
                    .or_else(|| arr.first())
            });
            let (file, line, column) = match span {
                Some(s) => (
                    s.get("file_name")
                        .and_then(|f| f.as_str())
                        .map(String::from),
                    s.get("line_start").and_then(|l| l.as_u64()),
                    s.get("column_start").and_then(|c| c.as_u64()),
                ),
                None => (None, None, None),
            };
            Some(Diagnostic {
                level,
                message: text,
                file,
                line,
                column,
            })
        })
        .collect()
}

/// The verdict of a verification run.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    /// Which build/test system ran (`cargo`, `go`, `npm`, `pytest`, `none`).
    pub system: String,
    pub ok: bool,
    pub errors: usize,
    pub warnings: usize,
    pub timed_out: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// A system-level failure reason with no structured diagnostics (e.g. no
    /// project manifest, or the tool isn't installed).
    pub failure: Option<String>,
}

/// Compile a Rust project and return a structured pass/fail report. This is the
/// verification loop the agent runs after every edit.
///
/// `ok` requires cargo to exit 0 — so a directory that isn't a cargo project (no
/// `Cargo.toml`) is correctly reported as a failure, not a vacuous success.
pub async fn verify_rust(cwd: impl Into<PathBuf>, timeout: Duration) -> Result<VerifyReport> {
    let spec = CommandSpec::new("cargo")
        .args(["build", "--message-format=json"])
        .cwd(cwd)
        .timeout(timeout);
    let out = run(&spec).await?;
    let diagnostics = parse_cargo_json(&out.stdout);
    let errors = diagnostics.iter().filter(|d| d.level == "error").count();
    let warnings = diagnostics.iter().filter(|d| d.level == "warning").count();
    let ok = out.code == Some(0) && errors == 0 && !out.timed_out;

    // A non-zero exit with no compiler errors means cargo itself failed (missing
    // manifest, resolver error, …); surface the first lines of its stderr.
    let failure = if !ok && errors == 0 && !out.timed_out {
        let reason = out
            .stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("cargo build failed")
            .to_string();
        Some(reason)
    } else {
        None
    };

    Ok(VerifyReport {
        system: "cargo".into(),
        ok,
        errors,
        warnings,
        timed_out: out.timed_out,
        diagnostics,
        failure,
    })
}

/// The build/test systems `verify` can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verifier {
    Cargo,
    Go,
    Npm,
    Pytest,
}

impl Verifier {
    pub fn name(self) -> &'static str {
        match self {
            Verifier::Cargo => "cargo",
            Verifier::Go => "go",
            Verifier::Npm => "npm",
            Verifier::Pytest => "pytest",
        }
    }

    /// Marker files that identify the project (checked in priority order).
    fn markers(self) -> &'static [&'static str] {
        match self {
            Verifier::Cargo => &["Cargo.toml"],
            Verifier::Go => &["go.mod"],
            Verifier::Npm => &["package.json"],
            Verifier::Pytest => &["pyproject.toml", "setup.py", "pytest.ini", "tox.ini"],
        }
    }

    /// The command run to check the project.
    fn command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Verifier::Cargo => ("cargo", &["build", "--message-format=json"]),
            Verifier::Go => ("go", &["build", "./..."]),
            Verifier::Npm => ("npm", &["test"]),
            Verifier::Pytest => ("pytest", &["-q"]),
        }
    }

    /// Detect the project type by scanning `dir` for marker files.
    pub fn detect(dir: &std::path::Path) -> Option<Verifier> {
        [
            Verifier::Cargo,
            Verifier::Go,
            Verifier::Npm,
            Verifier::Pytest,
        ]
        .into_iter()
        .find(|v| v.markers().iter().any(|m| dir.join(m).exists()))
    }
}

/// Auto-detect the project's build/test system and run it, returning a structured
/// pass/fail report. Rust gets rich cargo diagnostics; the others are exit-code
/// based with the failing output attached.
pub async fn verify(cwd: impl Into<PathBuf>, timeout: Duration) -> Result<VerifyReport> {
    let cwd = cwd.into();
    match Verifier::detect(&cwd) {
        Some(Verifier::Cargo) => verify_rust(cwd, timeout).await,
        Some(v) => verify_generic(v, &cwd, timeout).await,
        None => Ok(VerifyReport {
            system: "none".into(),
            ok: false,
            errors: 0,
            warnings: 0,
            timed_out: false,
            diagnostics: Vec::new(),
            failure: Some(
                "no recognized project here (looked for Cargo.toml, go.mod, package.json, pyproject.toml)"
                    .into(),
            ),
        }),
    }
}

async fn verify_generic(
    v: Verifier,
    cwd: &std::path::Path,
    timeout: Duration,
) -> Result<VerifyReport> {
    let (program, args) = v.command();
    let spec = CommandSpec::new(program)
        .args(args.iter().copied())
        .cwd(cwd)
        .timeout(timeout);
    let out = match run(&spec).await {
        Ok(o) => o,
        // The tool isn't installed — report it rather than erroring out.
        Err(ExecError::Spawn { program, .. }) => {
            return Ok(VerifyReport {
                system: v.name().into(),
                ok: false,
                errors: 0,
                warnings: 0,
                timed_out: false,
                diagnostics: Vec::new(),
                failure: Some(format!(
                    "could not run `{program}` — is it installed and on PATH?"
                )),
            })
        }
        Err(e) => return Err(e),
    };

    let ok = out.success();
    let failure = if ok {
        None
    } else if out.timed_out {
        Some(format!("`{}` timed out", v.name()))
    } else {
        // Prefer stderr, fall back to stdout; keep the last few lines (the summary).
        let text = if out.stderr.trim().is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        Some(last_lines(text, 20))
    };

    Ok(VerifyReport {
        system: v.name().into(),
        ok,
        errors: usize::from(!ok),
        warnings: 0,
        timed_out: out.timed_out,
        diagnostics: Vec::new(),
        failure,
    })
}

/// The last `n` non-empty lines of `text`, joined — a build tool's summary.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_stdout() {
        let out = run(&CommandSpec::new("echo").args(["hello", "harness"]))
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hello harness");
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported() {
        let out = run(&CommandSpec::new("sh").args(["-c", "exit 3"]))
            .await
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert!(!out.success());
    }

    #[tokio::test]
    async fn timeout_kills_the_group_and_returns_promptly() {
        let start = std::time::Instant::now();
        // The shell forks a child `sleep`; killing only the shell would leak it.
        // Killing the group reaps both, and we return right at the deadline.
        let out = run(&CommandSpec::new("sh")
            .args(["-c", "sleep 30 & sleep 30"])
            .timeout(Duration::from_millis(250)))
        .await
        .unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "did not return promptly"
        );
    }

    #[tokio::test]
    async fn does_not_hang_when_a_backgrounded_child_holds_the_pipe() {
        // The shell exits instantly but leaves a `sleep` holding stdout open.
        // Without tearing down the group, the drain would block for ~30s.
        let start = std::time::Instant::now();
        let out = run(&CommandSpec::new("sh")
            .args(["-c", "sleep 30 & echo hi"])
            .timeout(Duration::from_secs(60)))
        .await
        .unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(out.stdout.trim(), "hi");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "hung draining a backgrounded child's pipe"
        );
    }

    #[test]
    fn parses_cargo_compiler_message() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `x`","spans":[{"is_primary":true,"file_name":"src/lib.rs","line_start":10,"column_start":5}]}}"#;
        let noise = r#"{"reason":"build-script-executed"}"#;
        let diags = parse_cargo_json(&format!("{noise}\n{line}\n"));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].level, "error");
        assert_eq!(diags[0].file.as_deref(), Some("src/lib.rs"));
        assert_eq!(diags[0].line, Some(10));
    }

    #[tokio::test]
    async fn verify_flags_a_directory_that_is_not_a_cargo_project() {
        // A build that can't even start (no Cargo.toml) must not read as "ok".
        let dir = std::env::temp_dir().join(format!("sturdy-notcargo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let report = verify_rust(&dir, Duration::from_secs(60)).await.unwrap();
        assert!(!report.ok, "an empty directory must not verify as ok");
        assert!(
            report.failure.is_some(),
            "should report a cargo-level reason"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verifier_detects_by_marker_in_priority_order() {
        let dir = std::env::temp_dir().join(format!("sturdy-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Verifier::detect(&dir), None);
        std::fs::write(dir.join("go.mod"), "module x\n").unwrap();
        assert_eq!(Verifier::detect(&dir), Some(Verifier::Go));
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(Verifier::detect(&dir), Some(Verifier::Cargo)); // cargo wins
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn verify_reports_no_recognized_project() {
        let dir = std::env::temp_dir().join(format!("sturdy-noproj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = verify(&dir, Duration::from_secs(30)).await.unwrap();
        assert_eq!(r.system, "none");
        assert!(!r.ok);
        assert!(r.failure.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
