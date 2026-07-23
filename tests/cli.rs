//! End-to-end tests of the `aegis` binary, locking the CLI surface.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn aegis() -> Command {
    Command::cargo_bin("aegis").unwrap()
}

#[test]
fn help_lists_subcommands() {
    aegis()
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
    aegis()
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
    aegis()
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
    aegis()
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
    let assert = aegis()
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
    aegis()
        .args(["replay", task_id, "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(predicate::str::contains("finish"));
}

#[test]
fn verify_non_project_fails() {
    let dir = tempdir().unwrap();
    aegis()
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
    aegis()
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
    aegis()
        .args(["replay", "not-a-uuid", "--db"])
        .arg(&db)
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a valid task id"));
}
