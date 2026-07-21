//! # sturdy-exec
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

use sturdy_core::HarnessError;

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

/// Run `spec` to completion or until its timeout, capturing stdout/stderr. On
/// timeout the whole process group is killed and `timed_out` is set.
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

    let (code, timed_out) = match tokio::time::timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(source)) => {
            return Err(ExecError::Io {
                program: spec.program.clone(),
                source,
            })
        }
        Err(_) => {
            if let Some(p) = pid {
                kill_group(p);
            }
            let _ = child.wait().await; // reap the leader
            (None, true)
        }
    };

    let stdout = out_task
        .await
        .map_err(|_| ExecError::ReaderPanicked)?
        .map_err(|source| ExecError::Io {
            program: spec.program.clone(),
            source,
        })?;
    let stderr = err_task
        .await
        .map_err(|_| ExecError::ReaderPanicked)?
        .map_err(|source| ExecError::Io {
            program: spec.program.clone(),
            source,
        })?;

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
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub ok: bool,
    pub errors: usize,
    pub warnings: usize,
    pub timed_out: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// Compile a Rust project and return a structured pass/fail report. This is the
/// verification loop the agent runs after every edit.
pub async fn verify_rust(cwd: impl Into<PathBuf>, timeout: Duration) -> Result<VerifyReport> {
    let spec = CommandSpec::new("cargo")
        .args(["build", "--message-format=json"])
        .cwd(cwd)
        .timeout(timeout);
    let out = run(&spec).await?;
    let diagnostics = parse_cargo_json(&out.stdout);
    let errors = diagnostics.iter().filter(|d| d.level == "error").count();
    let warnings = diagnostics.iter().filter(|d| d.level == "warning").count();
    Ok(VerifyReport {
        ok: errors == 0 && !out.timed_out,
        errors,
        warnings,
        timed_out: out.timed_out,
        diagnostics,
    })
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
}
