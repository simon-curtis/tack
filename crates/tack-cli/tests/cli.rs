//! End-to-end integration tests for the `tack` CLI binary.
//!
//! Each test runs the real compiled binary (`env!("CARGO_BIN_EXE_tack")`) inside
//! a fresh temp directory used as the working copy, exercising the same
//! init → write → snap → log flow a user would. Tests are deterministic: they
//! assert on stable substrings, not on content-addressed ids.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Runs `tack <args...>` with `dir` as the working directory and returns the
/// captured output. A clean environment for the author identity keeps cuts
/// deterministic across machines.
fn run_tack(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(args)
        .current_dir(dir)
        .env("TACK_AUTHOR_NAME", "Tester")
        .env("TACK_AUTHOR_EMAIL", "tester@example.com")
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to launch the tack binary")
}

/// Asserts the command exited successfully, attaching stderr on failure.
fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn init_snap_log_round_trip() {
    let dir = TempDir::new().expect("temp dir");

    // init
    let out = run_tack(dir.path(), &["init"]);
    assert_ok(&out, "tack init");

    // Write a tracked file into the working copy.
    std::fs::write(dir.path().join("hello.txt"), b"world").expect("write file");

    // snap with a message → a named cut.
    let out = run_tack(dir.path(), &["snap", "-m", "first"]);
    assert_ok(&out, "tack snap -m first");

    // log should mention the cut message.
    let out = run_tack(dir.path(), &["log"]);
    assert_ok(&out, "tack log");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("first"),
        "log output should contain the cut message: {stdout}"
    );
}

#[test]
fn status_reports_added_file() {
    let dir = TempDir::new().expect("temp dir");
    assert_ok(&run_tack(dir.path(), &["init"]), "tack init");
    std::fs::write(dir.path().join("new.txt"), b"hi").expect("write file");

    let out = run_tack(dir.path(), &["status"]);
    assert_ok(&out, "tack status");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new.txt"),
        "status should list the added file: {stdout}"
    );
}

#[test]
fn json_log_emits_machine_readable_payload() {
    let dir = TempDir::new().expect("temp dir");
    assert_ok(&run_tack(dir.path(), &["init"]), "tack init");
    std::fs::write(dir.path().join("a.txt"), b"x").expect("write file");
    assert_ok(
        &run_tack(dir.path(), &["snap", "-m", "cut one"]),
        "tack snap",
    );

    let out = run_tack(dir.path(), &["log", "--json"]);
    assert_ok(&out, "tack log --json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("log --json must emit one JSON object");
    assert_eq!(
        value["status"], "log",
        "JSON payload should be a log response"
    );
    assert_eq!(value["cuts"][0]["message"], "cut one");
    // The organization data rule: no author e-mail in a log payload.
    assert!(
        !stdout.contains('@'),
        "log payload must not carry an e-mail: {stdout}"
    );
}

#[test]
fn op_log_lists_operations() {
    let dir = TempDir::new().expect("temp dir");
    assert_ok(&run_tack(dir.path(), &["init"]), "tack init");

    let out = run_tack(dir.path(), &["op", "log"]);
    assert_ok(&out, "tack op log");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("initialize repository"),
        "op log should contain the root op description: {stdout}"
    );
}

#[test]
fn cat_outside_a_repo_fails_with_nonzero_exit() {
    let dir = TempDir::new().expect("temp dir");
    // No `tack init` here → open should fail.
    let out = run_tack(dir.path(), &["status"]);
    assert!(
        !out.status.success(),
        "status outside a repo must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error:"),
        "a clear error message should be printed: {stderr}"
    );
}
