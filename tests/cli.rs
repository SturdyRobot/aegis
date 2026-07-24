//! End-to-end tests of the `kedge` binary, locking the CLI surface.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn kedge() -> Command {
    Command::cargo_bin("kedge").unwrap()
}

#[test]
fn help_lists_subcommands() {
    kedge()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("compact"))
        .stdout(predicate::str::contains("verify"))
        .stdout(predicate::str::contains("ledger"));
}

#[test]
fn compact_rust_file_elides_bodies() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("x.rs");
    fs::write(
        &f,
        "pub fn big() -> i32 {\n    let mut a = 0;\n    for i in 0..100 { a += i; }\n    a\n}\n",
    )
    .unwrap();
    kedge()
        .arg("compact")
        .arg(&f)
        .assert()
        .success()
        .stdout(predicate::str::contains("rust"))
        .stdout(predicate::str::contains("elided"))
        .stdout(predicate::str::contains("pub fn big() -> i32"))
        .stdout(predicate::str::contains("a += i").not());
}

#[test]
fn compact_detects_python_from_extension() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("x.py");
    fs::write(
        &f,
        "def big(n):\n    total = 0\n    for i in range(n):\n        total += i\n    return total\n",
    )
    .unwrap();
    kedge()
        .arg("compact")
        .arg(&f)
        .assert()
        .success()
        .stdout(predicate::str::contains("python"))
        .stdout(predicate::str::contains("def big(n):"));
}

#[test]
fn compact_unknown_extension_errors() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("data.txt");
    fs::write(&f, "hello").unwrap();
    kedge()
        .arg("compact")
        .arg(&f)
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported language"));
}

#[test]
fn run_json_finishes_and_journals() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("l.sqlite");
    let assert = kedge()
        .args(["run", "check the toolchain", "--json", "--db"])
        .arg(&db)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("run --json must be valid JSON");
    assert_eq!(v["outcome"]["status"], "finished");
    assert!(!v["trajectory"]["steps"].as_array().unwrap().is_empty());

    // The same run is now replayable from the ledger.
    let task_id = v["task_id"].as_str().unwrap();
    kedge()
        .args(["replay", task_id, "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(predicate::str::contains("finish"));
}

#[test]
fn verify_non_project_fails() {
    let dir = tempdir().unwrap();
    kedge()
        .arg("verify")
        .arg(dir.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("failed"));
}

#[test]
fn ledger_list_on_empty_db() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("l.sqlite");
    kedge()
        .args(["ledger", "list", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(predicate::str::contains("no runs recorded"));
}

#[test]
fn replay_malformed_id_errors() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("l.sqlite");
    kedge()
        .args(["replay", "not-a-uuid", "--db"])
        .arg(&db)
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a valid task id"));
}

/// Regression: a `tools/call` in flight when stdin closes must still get its
/// response written. Dispatching tool calls onto their own tasks (so a slow run
/// can't block the reader) originally let the runtime cancel them at shutdown,
/// silently dropping a response for work that had already completed.
#[test]
fn mcp_writes_responses_for_calls_in_flight_when_stdin_closes() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ledger.sqlite");

    let requests = format!(
        "{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "kedge_audit", "arguments": { "db": db.to_str().unwrap() } }
        })
    );

    let out = kedge().arg("mcp").write_stdin(requests).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    let ids: Vec<u64> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("id").and_then(|i| i.as_u64()))
        .collect();

    assert!(
        ids.contains(&1),
        "initialize must be answered; got {stdout}"
    );
    assert!(
        ids.contains(&2),
        "the tool call's response must survive stdin closing; got {stdout}"
    );
}

/// The MCP handshake gates tool traffic: `tools/list` before `initialize` is an
/// error, not a silent success.
#[test]
fn mcp_rejects_tool_traffic_before_initialize() {
    let out = kedge()
        .arg("mcp")
        .write_stdin("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n")
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(
        v["error"]["code"], -32002,
        "expected not-initialized; got {stdout}"
    );
}
