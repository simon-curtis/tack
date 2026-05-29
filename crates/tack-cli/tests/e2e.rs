//! Full-lifecycle end-to-end integration test for the `tack` CLI binary.
//!
//! Unlike `cli.rs` (which exercises individual verbs in isolation), this test
//! drives the *real* compiled binary (`env!("CARGO_BIN_EXE_tack")`) through one
//! realistic session in a single temp working copy:
//!
//! ```text
//! init → create files + subdirs → status (adds) → snap A → edit → status (mod)
//!      → snap B → log (newest-first) → diff A..B → restore --to A (non-destructive)
//!      → undo → cat/ls an object → .tackignore → agent `serve` channel
//! ```
//!
//! It is deterministic: no sleeps, no reliance on wall-clock ordering. Cut order
//! is asserted from the engine's own newest-first contract, and every id used
//! for `restore`/`cat`/`ls` is read back out of a prior `--json` response rather
//! than guessed. The author identity is pinned via env so cuts are reproducible.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

// ── harness ──────────────────────────────────────────────────────────────────

/// Runs `tack <args...>` with `dir` as the working directory, feeding `stdin`
/// (empty for the common case) and returning the captured output. A pinned
/// author identity and a cleared `RUST_LOG` keep results deterministic.
fn run_tack_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> Output {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(args)
        .current_dir(dir)
        .env("TACK_AUTHOR_NAME", "Tester")
        .env("TACK_AUTHOR_EMAIL", "tester@example.com")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to launch the tack binary");

    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin)
        .expect("write child stdin");
    // stdin handle drops here → EOF, so commands that read stdin (serve) stop.

    child.wait_with_output().expect("wait for tack")
}

/// Runs `tack <args...>` with no stdin.
fn run_tack(dir: &Path, args: &[&str]) -> Output {
    run_tack_stdin(dir, args, b"")
}

/// Asserts the command exited successfully, attaching stderr on failure.
fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs a read command with `--json` and parses the single JSON object it emits.
fn run_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = args.to_vec();
    full.push("--json");
    let out = run_tack(dir, &full);
    assert_ok(&out, &format!("tack {} --json", args.join(" ")));
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "`tack {}` must emit one JSON object ({e}): {stdout}",
            args.join(" ")
        )
    })
}

/// Writes `content` to `root/rel`, creating any parent directories.
fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdirs");
    }
    std::fs::write(path, content).expect("write file");
}

/// Reads `cuts[].message` from a `Log` JSON response, in wire order.
fn cut_messages(log: &Value) -> Vec<String> {
    log["cuts"]
        .as_array()
        .expect("cuts array")
        .iter()
        .map(|c| c["message"].as_str().expect("message").to_owned())
        .collect()
}

// ── the full lifecycle ───────────────────────────────────────────────────────

#[test]
fn full_lifecycle_drives_the_binary_end_to_end() {
    let tmp = TempDir::new().expect("temp dir");
    let dir = tmp.path();

    // ── init ──────────────────────────────────────────────────────────────────
    assert_ok(&run_tack(dir, &["init"]), "tack init");
    assert!(dir.join(".tack").is_dir(), "init must create .tack/");

    // Re-initialising an existing repo must fail (no silent clobber).
    assert!(
        !run_tack(dir, &["init"]).status.success(),
        "init over an existing repo must exit non-zero"
    );

    // ── create files + subdirs ──────────────────────────────────────────────────
    write_file(dir, "root.txt", b"root v1\n");
    write_file(dir, "src/lib.rs", b"pub fn one() -> u32 { 1 }\n");
    write_file(dir, "src/nested/deep.txt", b"deep v1\n");

    // ── status: all three show as added ──────────────────────────────────────────
    let status = run_json(dir, &["status"]);
    assert_eq!(status["status"], "status");
    assert_eq!(
        status["data"]["clean"], false,
        "a freshly-populated tree is not clean"
    );
    let added: Vec<&str> = status["data"]["added"]
        .as_array()
        .expect("added array")
        .iter()
        .map(|v| v.as_str().expect("path"))
        .collect();
    for want in ["root.txt", "src/lib.rs", "src/nested/deep.txt"] {
        assert!(
            added.contains(&want),
            "status adds should include {want}: {added:?}"
        );
    }

    // ── snap -m A ────────────────────────────────────────────────────────────────
    let snap_a = run_json(dir, &["snap", "-m", "A"]);
    assert_eq!(snap_a["status"], "named_cut");
    let cut_a = snap_a["cut"].as_str().expect("cut A id").to_owned();
    assert_eq!(cut_a.len(), 64, "cut id should be a 64-char hex object id");

    // After a cut the working copy matches its snapshot → clean.
    let status = run_json(dir, &["status"]);
    assert_eq!(
        status["data"]["clean"], true,
        "after snap A the working copy is clean"
    );

    // ── edit a file ──────────────────────────────────────────────────────────────
    write_file(dir, "root.txt", b"root v2 EDITED\n");

    // ── status: the edited file shows as modified, nothing added ──────────────────
    let status = run_json(dir, &["status"]);
    assert_eq!(
        status["data"]["clean"], false,
        "an edit makes the working copy dirty"
    );
    let modified: Vec<&str> = status["data"]["modified"]
        .as_array()
        .expect("modified array")
        .iter()
        .map(|v| v.as_str().expect("path"))
        .collect();
    assert_eq!(
        modified,
        vec!["root.txt"],
        "only root.txt should be modified: {status}"
    );
    assert!(
        status["data"]["added"]
            .as_array()
            .expect("added")
            .is_empty(),
        "no new adds after an edit: {status}"
    );

    // ── snap -m B ────────────────────────────────────────────────────────────────
    let snap_b = run_json(dir, &["snap", "-m", "B"]);
    let cut_b = snap_b["cut"].as_str().expect("cut B id").to_owned();
    assert_ne!(cut_a, cut_b, "A and B must be distinct cuts");

    // ── log: both cuts, newest-first (B then A) ──────────────────────────────────
    let log = run_json(dir, &["log"]);
    assert_eq!(log["status"], "log");
    let messages = cut_messages(&log);
    assert_eq!(
        messages,
        vec!["B", "A"],
        "log must list cuts newest-first: {messages:?}"
    );
    // The newest cut's id matches what `snap B` returned, and it parents A.
    assert_eq!(log["cuts"][0]["id"].as_str().unwrap(), cut_b);
    let b_parents: Vec<&str> = log["cuts"][0]["parents"]
        .as_array()
        .expect("parents")
        .iter()
        .map(|v| v.as_str().expect("parent id"))
        .collect();
    assert!(
        b_parents.contains(&cut_a.as_str()),
        "B should parent A: {b_parents:?}"
    );
    // Organization data rule: no author e-mail anywhere in the log payload.
    let log_text = serde_json::to_string(&log).unwrap();
    assert!(
        !log_text.contains('@'),
        "log payload must not carry an e-mail: {log_text}"
    );

    // ── diff between cuts A..B → root.txt modified ────────────────────────────────
    let diff = run_json(dir, &["diff", "--from", &cut_a, "--to", &cut_b]);
    assert_eq!(diff["status"], "diff");
    // An explicit `--to` snapshot is reported as such for self-explanatory logs.
    assert_eq!(
        diff["to_kind"], "snapshot",
        "explicit --to is a recorded snapshot: {diff}"
    );
    let diff_mod: Vec<&str> = diff["data"]["modified"]
        .as_array()
        .expect("modified")
        .iter()
        .map(|v| v.as_str().expect("path"))
        .collect();
    assert_eq!(
        diff_mod,
        vec!["root.txt"],
        "diff A..B should report root.txt modified: {diff}"
    );
    assert!(
        diff["data"]["added"].as_array().unwrap().is_empty()
            && diff["data"]["removed"].as_array().unwrap().is_empty(),
        "diff A..B should have no adds/removes: {diff}"
    );

    // ── op log BEFORE restore: capture the head op so we can prove it survives ────
    let op_log_before = run_json(dir, &["op", "log"]);
    let ops_before: Vec<String> = op_log_before["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .map(|o| o["id"].as_str().expect("op id").to_owned())
        .collect();
    let pre_restore_head = ops_before.first().expect("at least one op").clone();
    let op_count_before = ops_before.len();

    // ── restore --to A (non-destructive) ──────────────────────────────────────────
    assert_ok(
        &run_tack(dir, &["restore", "--to", &cut_a]),
        "tack restore --to A",
    );

    // Working dir now matches A: root.txt is back to v1, and a diff of the
    // working copy against A is empty (clean).
    let restored = std::fs::read(dir.join("root.txt")).expect("read root.txt");
    assert_eq!(
        restored, b"root v1\n",
        "root.txt must be restored to A's content"
    );
    let status_after_restore = run_json(dir, &["status"]);
    assert_eq!(
        status_after_restore["data"]["clean"], true,
        "after restore --to A the working copy should match A exactly: {status_after_restore}"
    );

    // Non-destructive: the op log STILL contains the pre-restore head op, and the
    // restore appended a new op on top (count grew, head changed).
    let op_log_after = run_json(dir, &["op", "log"]);
    let ops_after: Vec<String> = op_log_after["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .map(|o| o["id"].as_str().expect("op id").to_owned())
        .collect();
    assert!(
        ops_after.contains(&pre_restore_head),
        "restore must be non-destructive: pre-restore op {pre_restore_head} must still be reachable: {ops_after:?}"
    );
    assert!(
        ops_after.len() > op_count_before,
        "restore must append a new op (had {op_count_before}, now {}): {ops_after:?}",
        ops_after.len()
    );
    assert_ne!(
        ops_after.first(),
        Some(&pre_restore_head),
        "restore should advance the op head to a new op"
    );

    // ── undo: reverts the restore and adds yet another op ─────────────────────────
    assert_ok(&run_tack(dir, &["undo"]), "tack undo");

    // Undo reinstates the pre-restore working copy → root.txt is the edited v2,
    // i.e. the working copy now matches B again.
    let after_undo = std::fs::read(dir.join("root.txt")).expect("read root.txt");
    assert_eq!(
        after_undo, b"root v2 EDITED\n",
        "undo must revert the restore (back to B's content)"
    );
    let status_after_undo = run_json(dir, &["status"]);
    assert_eq!(
        status_after_undo["data"]["clean"], true,
        "after undo the working copy should match B exactly: {status_after_undo}"
    );

    let op_log_final = run_json(dir, &["op", "log"]);
    let final_count = op_log_final["ops"].as_array().expect("ops").len();
    assert!(
        final_count > ops_after.len(),
        "undo must append an op (had {}, now {final_count})",
        ops_after.len()
    );

    // ── cat: inspect the cut A object (a snapshot) ────────────────────────────────
    let cat = run_json(dir, &["cat", &cut_a]);
    assert_eq!(cat["status"], "cat");
    assert_eq!(cat["object"]["id"].as_str().unwrap(), cut_a);
    assert_eq!(
        cat["object"]["kind"], "snapshot",
        "cut A is a snapshot object"
    );
    assert!(
        cat["object"]["size"].as_u64().unwrap() > 0,
        "snapshot has a non-zero byte length"
    );

    // ── ls: list the working-copy root tree (matches B again after undo) ──────────
    let ls = run_json(dir, &["ls"]);
    assert_eq!(ls["status"], "ls");
    let entries: Vec<(&str, &str)> = ls["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["kind"].as_str().unwrap()))
        .collect();
    assert!(
        entries.contains(&("root.txt", "blob")),
        "ls root should list root.txt as a blob: {entries:?}"
    );
    assert!(
        entries.contains(&("src", "tree")),
        "ls root should list the src subtree: {entries:?}"
    );

    // ── agent channel: drive `tack serve` over stdin/stdout ───────────────────────
    // Pipe two JSON-RPC requests; assert two valid JSON responses, in order, with
    // no author e-mail anywhere in the stream.
    let requests = "{\"method\":\"log\"}\n{\"method\":\"status\"}\n";
    let serve_out = run_tack_stdin(dir, &["serve"], requests.as_bytes());
    assert_ok(&serve_out, "tack serve");
    let serve_text = String::from_utf8(serve_out.stdout).expect("utf8 serve output");
    let lines: Vec<&str> = serve_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "serve must answer one JSON line per request: {serve_text}"
    );

    let r_log: Value = serde_json::from_str(lines[0]).expect("serve line 1 is JSON");
    assert_eq!(
        r_log["status"], "log",
        "first serve response should be a log"
    );
    assert_eq!(
        cut_messages(&r_log),
        vec!["B", "A"],
        "serve log matches CLI log, newest-first"
    );

    let r_status: Value = serde_json::from_str(lines[1]).expect("serve line 2 is JSON");
    assert_eq!(
        r_status["status"], "status",
        "second serve response should be a status"
    );
    assert_eq!(
        r_status["data"]["clean"], true,
        "serve status should report a clean working copy"
    );

    assert!(
        !serve_text.contains('@'),
        "agent serve responses must not leak an author e-mail: {serve_text}"
    );
    assert!(
        !serve_text.contains("tester@example.com"),
        "the pinned author e-mail must never appear on the agent channel"
    );
}

// ── .tackignore ───────────────────────────────────────────────────────────────

#[test]
fn tackignore_excludes_matching_files_from_tracking() {
    let tmp = TempDir::new().expect("temp dir");
    let dir = tmp.path();
    assert_ok(&run_tack(dir, &["init"]), "tack init");

    // Ignore *.log everywhere, plus a whole build/ directory.
    write_file(dir, ".tackignore", b"*.log\nbuild/\n");

    // A tracked file, an ignored log, and an ignored build artifact.
    write_file(dir, "keep.txt", b"keep me\n");
    write_file(dir, "debug.log", b"noisy\n");
    write_file(dir, "build/out.bin", b"\x00\x01\x02");

    // status: only keep.txt and .tackignore should appear as added; never the
    // ignored paths.
    let status = run_json(dir, &["status"]);
    let added: Vec<&str> = status["data"]["added"]
        .as_array()
        .expect("added")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        added.contains(&"keep.txt"),
        "tracked file must be added: {added:?}"
    );
    assert!(
        !added.iter().any(|p| p.ends_with(".log")),
        "an ignored *.log file must not be tracked: {added:?}"
    );
    assert!(
        !added.iter().any(|p| p.starts_with("build/")),
        "files under an ignored build/ dir must not be tracked: {added:?}"
    );

    // Snapshot, then confirm the ignored content never reached the tree either.
    assert_ok(&run_tack(dir, &["snap", "-m", "tracked only"]), "tack snap");
    let ls = run_json(dir, &["ls"]);
    let names: Vec<&str> = ls["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"keep.txt"),
        "ls should list the tracked file: {names:?}"
    );
    assert!(
        !names.contains(&"debug.log"),
        "ls must not list the ignored log: {names:?}"
    );
    assert!(
        !names.contains(&"build"),
        "ls must not list the ignored build dir: {names:?}"
    );
}

// ── agent-facing v0.2 flow: orientation, coordination, content diff ───────────

/// Drives the real binary through the agent ergonomics added for the feedback
/// pass: `schema`, `current`, advisory `claim`/`claims`, content-level
/// `diff --patch`/`--stat`, a scoped cut, structured `restore`, and `cuts`
/// (all-lineage). Every agent payload is checked to be free of author e-mail.
#[test]
fn agent_orientation_coordination_and_content_diff_flow() {
    let tmp = TempDir::new().expect("temp dir");
    let dir = tmp.path();
    assert_ok(&run_tack(dir, &["init"]), "tack init");

    // ── schema: the API is self-describing (and needs no special repo state) ──
    let schema = run_json(dir, &["schema"]);
    assert_eq!(schema["status"], "help");
    let methods: Vec<&str> = schema["methods"]
        .as_array()
        .expect("methods array")
        .iter()
        .map(|m| m["method"].as_str().expect("method name"))
        .collect();
    for want in [
        "current",
        "cuts",
        "claim",
        "release",
        "claims",
        "scoped_cut",
        "diff",
    ] {
        assert!(
            methods.contains(&want),
            "schema must list {want}: {methods:?}"
        );
    }

    // ── base content + cut ────────────────────────────────────────────────────
    write_file(dir, "src/model.txt", b"line1\nline2\n");
    write_file(dir, "other/data.txt", b"keep\n");
    let base = run_json(dir, &["snap", "-m", "base"]);
    assert_eq!(base["status"], "named_cut");
    let base_cut = base["cut"].as_str().expect("base cut id").to_owned();
    assert_eq!(base["cut_short"].as_str().expect("short").len(), 12);

    // ── current: clean, not a restore, base cut present, short ids present ─────
    let cur = run_json(dir, &["current"]);
    assert_eq!(cur["status"], "current");
    assert_eq!(cur["data"]["from_restore"], false);
    assert_eq!(cur["data"]["clean"], true);
    assert_eq!(cur["data"]["base_cut"]["id"].as_str().unwrap(), base_cut);
    assert_eq!(
        cur["data"]["working_copy_short"].as_str().unwrap().len(),
        12
    );

    // ── advisory claims: a path claim, then an overlapping dir claim conflicts ─
    let claimed = run_json(
        dir,
        &["claim", "src/model.txt", "--as", "alice", "--note", "wip"],
    );
    assert_eq!(claimed["status"], "claimed");
    assert_eq!(claimed["claim"]["holder"], "alice");
    assert!(
        claimed["conflicts"].as_array().unwrap().is_empty(),
        "first claim has no conflict"
    );

    let claimed2 = run_json(dir, &["claim", "src", "--as", "bob"]);
    let conflicts = claimed2["conflicts"].as_array().expect("conflicts array");
    assert_eq!(
        conflicts.len(),
        1,
        "bob's dir claim overlaps alice's file claim: {claimed2}"
    );
    assert_eq!(conflicts[0]["holder"], "alice");

    let claims = run_json(dir, &["claims"]);
    assert_eq!(
        claims["claims"].as_array().unwrap().len(),
        2,
        "two advisory claims held"
    );

    // ── content-level diff after an edit ──────────────────────────────────────
    write_file(dir, "src/model.txt", b"line1\nCHANGED\n");
    assert_ok(&run_tack(dir, &["snap"]), "auto-snapshot the edit");
    let patch = run_json(dir, &["diff", "--patch"]);
    assert_eq!(patch["status"], "diff_patch");
    // A default diff reports the live working copy as its `to` side.
    assert_eq!(
        patch["to_kind"], "live_working_copy",
        "default diff is vs live disk: {patch}"
    );
    let patched = patch["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"] == "src/model.txt")
        .expect("a patch for src/model.txt");
    assert!(
        !patched["hunks"].as_array().unwrap().is_empty(),
        "a text change must yield hunks"
    );

    let stat = run_json(dir, &["diff", "--stat"]);
    assert_eq!(stat["status"], "diff_stat");
    assert_eq!(
        stat["to_kind"], "live_working_copy",
        "default --stat is vs live disk: {stat}"
    );
    assert!(
        stat["total_added"].as_u64().unwrap() >= 1,
        "stat must count inserted lines: {stat}"
    );

    // ── scoped cut: capture only src/, leaving a peer's other/ edit behind ────
    write_file(dir, "other/data.txt", b"peer-edit\n"); // an uncaptured concurrent edit
    let scoped = run_json(dir, &["snap", "-m", "scoped src", "--only", "src"]);
    assert_eq!(scoped["status"], "scoped_cut");
    let captured: Vec<&str> = scoped["data"]["captured"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        captured.contains(&"src/model.txt"),
        "captured must include the scoped file: {captured:?}"
    );
    let outside: Vec<&str> = scoped["data"]["outside_changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        outside.contains(&"other/data.txt"),
        "outside_changes must flag the uncaptured peer edit: {outside:?}"
    );

    // ── structured restore back to base ───────────────────────────────────────
    let restored = run_json(dir, &["restore", "--to", &base_cut]);
    assert_eq!(restored["status"], "restored");
    assert!(
        restored["previous_op"].as_str().is_some(),
        "restore reports the previous op"
    );
    assert_eq!(
        restored["restored_cut"]["id"].as_str().unwrap(),
        base_cut,
        "restoring to a named cut reports that cut"
    );

    // ── cuts (all lineages) surfaces the off-lineage scoped cut log now hides ─
    let cuts = run_json(dir, &["cuts"]);
    assert_eq!(cuts["status"], "cuts");
    let cut_msgs: Vec<&str> = cuts["cuts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["message"].as_str().unwrap())
        .collect();
    assert!(
        cut_msgs.contains(&"scoped src"),
        "cuts must surface the off-lineage scoped cut: {cut_msgs:?}"
    );

    // ── current after a restore reports from_restore = true ───────────────────
    let cur2 = run_json(dir, &["current"]);
    assert_eq!(
        cur2["data"]["from_restore"], true,
        "after restore from_restore must be true"
    );

    // ── organization data rule: no agent payload carries an author e-mail ─────
    for args in [
        vec!["current"],
        vec!["cuts"],
        vec!["claims"],
        vec!["schema"],
    ] {
        let value = run_json(dir, &args);
        let text = serde_json::to_string(&value).unwrap();
        assert!(
            !text.contains('@'),
            "`tack {}` payload must not carry an e-mail: {text}",
            args.join(" ")
        );
    }
}
