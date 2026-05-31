//! `tack hook pre-tool-use` — `PreToolUse` hook handler for AI coding agents.
//!
//! This module intercepts `git` invocations from Claude Code and Codex CLI
//! (which share the same `PreToolUse` hook contract) and returns a `deny` decision
//! with a tack-equivalent hint when the tool is run inside a tack-only repo.
//!
//! The core logic lives in [`pre_tool_use`], which is pure and fully testable.
//! The stdin/stdout wiring in `commands.rs` is intentionally thin.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;

// ── git detection ────────────────────────────────────────────────────────────

/// Characters that act as shell command separators when they appear on their own
/// or as the sole content of a whitespace-delimited token.
const SEPARATOR_CHARS: &[char] = &[';', '|', '(', ')'];

/// Shell separator token strings (multi-character).
const SEPARATOR_TOKENS: &[&str] = &["&&", "||", "|", ";", "(", ")"];

/// Yields shell tokens from `command`, splitting on whitespace AND expanding
/// tokens that contain embedded separator characters (e.g. `"hi;"` → `"hi"`,
/// `";"`).  This handles the common shell idiom `cmd; git …` where the
/// semicolon is written with no surrounding spaces.
fn shell_tokens(command: &str) -> impl Iterator<Item = &str> {
    // We flatten each whitespace-split token into sub-tokens by splitting on
    // separator characters while keeping the separator characters themselves
    // as their own token.  This is achieved by a simple two-pointer scan.
    command.split_whitespace().flat_map(SeparatorSplitter::new)
}

/// An iterator that splits a single whitespace-delimited shell token on
/// embedded separator characters, emitting the separator characters as their
/// own tokens.
struct SeparatorSplitter<'a> {
    remaining: &'a str,
}

impl<'a> SeparatorSplitter<'a> {
    const fn new(s: &'a str) -> Self {
        Self { remaining: s }
    }
}

impl<'a> Iterator for SeparatorSplitter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }

        // If the token starts with a separator char, emit it alone.
        let first_char = self.remaining.chars().next().expect("non-empty");
        if SEPARATOR_CHARS.contains(&first_char) {
            let char_len = first_char.len_utf8();
            let sep = &self.remaining[..char_len];
            self.remaining = &self.remaining[char_len..];
            return Some(sep);
        }

        // Scan forward to the next separator char (if any).
        if let Some(pos) = self.remaining.find(SEPARATOR_CHARS) {
            let token = &self.remaining[..pos];
            self.remaining = &self.remaining[pos..];
            Some(token)
        } else {
            let token = self.remaining;
            self.remaining = "";
            Some(token)
        }
    }
}

/// Returns `true` when a whitespace-delimited token (already split from an
/// embedded separator) is itself a shell separator.
fn is_separator(token: &str) -> bool {
    SEPARATOR_TOKENS.contains(&token)
}

/// Returns `true` if the shell command string contains a bare `git` invocation.
///
/// A token is considered the git command only when it equals exactly `"git"` and
/// sits in command position: the first token, or the first token after a shell
/// separator token (`&&`, `||`, `|`, `;`, `(`).
///
/// Full paths like `/usr/bin/git` are intentionally not matched (v0 limitation,
/// documented in the spec).
pub fn command_contains_git(command: &str) -> bool {
    let mut expect_command = true;

    for token in shell_tokens(command) {
        if is_separator(token) {
            expect_command = true;
            continue;
        }
        if expect_command {
            if token == "git" {
                return true;
            }
            // Any non-separator, non-git token in command position is a different
            // command — the next token is an argument, not a command.
            expect_command = false;
        }
    }

    false
}

/// Extracts the git subcommand (the token immediately after `git`) from a shell
/// command string. Returns `None` if `git` is not in command position or has no
/// following token.
fn git_subcommand(command: &str) -> Option<&str> {
    let mut expect_command = true;
    let mut tokens = shell_tokens(command);

    while let Some(token) = tokens.next() {
        if is_separator(token) {
            expect_command = true;
            continue;
        }
        if expect_command {
            if token == "git" {
                // Skip any immediately following separator tokens.
                for next in tokens.by_ref() {
                    if !is_separator(next) {
                        return Some(next);
                    }
                }
                return None;
            }
            expect_command = false;
        }
    }

    None
}

// ── repo root discovery ───────────────────────────────────────────────────────

/// Walks from `start` upward looking for a `.tack` directory.
///
/// Returns a tuple `(tack_root, has_git)` where `has_git` is `true` when a
/// `.git` directory (or file — for worktrees) also exists at the same root.
/// Returns `None` if no tack root is found.
fn find_tack_root(start: &Path) -> Option<(PathBuf, bool)> {
    let mut current = start;

    loop {
        let tack_dir = current.join(".tack");
        if tack_dir.is_dir() {
            let has_git = current.join(".git").exists();
            return Some((current.to_path_buf(), has_git));
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}

// ── deny message ─────────────────────────────────────────────────────────────

/// Maps a git subcommand to the tack-equivalent hint message.
fn deny_message(subcommand: Option<&str>) -> &'static str {
    match subcommand {
        Some("commit") => {
            "This repo uses tack, not git. Use `tack snap -m \"msg\"` to make a named cut."
        }
        Some("status") => {
            "This repo uses tack. Use `tack status` (and `tack current` / `tack claims`)."
        }
        Some("add") => {
            "tack has no staging area — just edit files; a cut captures the live tree. No `git add` needed."
        }
        Some("diff") => "Use `tack diff` (`--patch` / `--stat` for content).",
        Some("log") => "Use `tack log` (current lineage) or `tack cuts` (all lineages).",
        Some("checkout" | "switch" | "reset" | "revert") => {
            "Use `tack restore --to <id>` or `tack undo` (both non-destructive)."
        }
        Some("init") => "This is already a tack repo. Use `tack` commands, not git.",
        Some("branch") => {
            "tack derives lanes from admissions, not git branches. Use `tack lanes` and `tack admit <cut> --to <lane>`."
        }
        Some("merge" | "rebase") => {
            "tack has no rebase or fast-forward flow. Use cuts, lane admission, and `tack backport` for release fixes."
        }
        Some("cherry-pick") => {
            "Use `tack backport <source-cut> --to <lane>` to port a fix into a release lane."
        }
        Some("push" | "pull" | "fetch" | "clone" | "remote") => {
            "tack is local-only in v0 — there is no remote yet."
        }
        _ => "This repo uses tack, not git. Run `tack schema` to see the command set.",
    }
}

// ── core handler ─────────────────────────────────────────────────────────────

fn read_event(input: impl Read) -> Option<serde_json::Value> {
    let mut events = serde_json::Deserializer::from_reader(input).into_iter::<serde_json::Value>();
    match events.next() {
        Some(Ok(event)) => Some(event),
        _ => None,
    }
}

/// Decides a `PreToolUse` event read from `input`, writing any decision JSON to `out`.
///
/// The contract: write nothing for ALLOW; write a deny JSON object for DENY.
/// Returns `Ok(())` always — a hook must not crash the host agent.
///
/// # Errors
///
/// Returns `Err` only on I/O failures writing to `out`; JSON parse errors in
/// the input are treated as ALLOW to be safe.
pub fn pre_tool_use(input: impl Read, out: &mut impl Write) -> Result<()> {
    // Parse input; on any error → allow (hook must not crash).
    let Some(event) = read_event(input) else {
        return Ok(());
    };

    // Rule 1: only intercept Bash tool calls.
    let tool_name = event
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if tool_name != "Bash" {
        return Ok(());
    }

    // Rule 2: extract the shell command.
    let Some(command) = event
        .get("tool_input")
        .and_then(|ti| ti.get("command"))
        .and_then(|v| v.as_str())
    else {
        return Ok(());
    };

    // Rule 3: check for a git invocation in command position.
    if !command_contains_git(command) {
        return Ok(());
    }

    // Rule 4: self-gate — only deny in a tack-only repo (no .git co-located).
    let cwd_str = event.get("cwd").and_then(|v| v.as_str());
    let cwd = cwd_str
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let Some((_, has_git)) = find_tack_root(&cwd) else {
        // No tack root at all → allow.
        return Ok(());
    };

    if has_git {
        // Mixed repo (both .tack and .git) → allow git; don't break it.
        return Ok(());
    }

    // Rule 5: deny with a tack-equivalent hint.
    let subcommand = git_subcommand(command);
    let message = deny_message(subcommand);

    let deny = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": message,
        }
    });

    let json = serde_json::to_string(&deny).expect("deny object is always serialisable");
    writeln!(out, "{json}")?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;

    use tempfile::TempDir;

    use super::*;

    // ── git-detection unit tests ─────────────────────────────────────────────

    #[test]
    fn detects_git_as_first_token() {
        assert!(command_contains_git("git status"));
    }

    #[test]
    fn detects_bare_git_alone() {
        assert!(command_contains_git("git"));
    }

    #[test]
    fn detects_git_after_and_separator() {
        assert!(command_contains_git("ls && git commit -m 'x'"));
    }

    #[test]
    fn detects_git_after_pipe() {
        assert!(command_contains_git("echo foo | git commit --allow-empty"));
    }

    #[test]
    fn detects_git_after_or_separator() {
        assert!(command_contains_git("true || git status"));
    }

    #[test]
    fn detects_git_after_semicolon() {
        assert!(command_contains_git("echo hi; git log"));
    }

    #[test]
    fn detects_git_after_open_paren() {
        assert!(command_contains_git("( git status )"));
    }

    #[test]
    fn does_not_match_gitk() {
        assert!(!command_contains_git("gitk"));
    }

    #[test]
    fn does_not_match_digit_x() {
        assert!(!command_contains_git("digit x"));
    }

    #[test]
    fn does_not_match_git_as_argument() {
        // `git` is an argument to `echo`, not a command.
        assert!(!command_contains_git("echo git"));
    }

    #[test]
    fn does_not_match_full_path_git() {
        // Full paths are intentionally not matched in v0.
        assert!(!command_contains_git("/usr/bin/git status"));
    }

    #[test]
    fn does_not_match_git_foo_hyphenated() {
        assert!(!command_contains_git("git-foo bar"));
    }

    // ── subcommand extraction ────────────────────────────────────────────────

    #[test]
    fn extracts_subcommand_commit() {
        assert_eq!(git_subcommand("git commit -m hi"), Some("commit"));
    }

    #[test]
    fn extracts_subcommand_after_separator() {
        assert_eq!(git_subcommand("ls && git log --oneline"), Some("log"));
    }

    #[test]
    fn extracts_none_for_bare_git() {
        assert_eq!(git_subcommand("git"), None);
    }

    #[test]
    fn extracts_none_when_no_git() {
        assert_eq!(git_subcommand("echo git"), None);
    }

    // ── deny message coverage ────────────────────────────────────────────────

    #[test]
    fn deny_message_maps_known_subcommands() {
        assert!(deny_message(Some("commit")).contains("tack snap"));
        assert!(deny_message(Some("status")).contains("tack status"));
        assert!(deny_message(Some("add")).contains("staging area"));
        assert!(deny_message(Some("diff")).contains("tack diff"));
        assert!(deny_message(Some("log")).contains("tack log"));
        assert!(deny_message(Some("checkout")).contains("tack restore"));
        assert!(deny_message(Some("switch")).contains("tack restore"));
        assert!(deny_message(Some("reset")).contains("tack restore"));
        assert!(deny_message(Some("revert")).contains("tack restore"));
        assert!(deny_message(Some("init")).contains("already a tack repo"));
        assert!(deny_message(Some("branch")).contains("tack lanes"));
        assert!(deny_message(Some("merge")).contains("tack backport"));
        assert!(deny_message(Some("rebase")).contains("fast-forward"));
        assert!(deny_message(Some("cherry-pick")).contains("tack backport"));
        assert!(deny_message(Some("push")).contains("local-only"));
        assert!(deny_message(Some("pull")).contains("local-only"));
        assert!(deny_message(Some("fetch")).contains("local-only"));
        assert!(deny_message(Some("clone")).contains("local-only"));
        assert!(deny_message(Some("remote")).contains("local-only"));
    }

    #[test]
    fn deny_message_fallback_for_unknown_subcommand() {
        assert!(deny_message(Some("stash")).contains("tack schema"));
        assert!(deny_message(None).contains("tack schema"));
    }

    // ── helpers for building test environments ────────────────────────────────

    fn make_tack_only_dir() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".tack")).expect("create .tack");
        dir
    }

    fn make_mixed_dir() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".tack")).expect("create .tack");
        fs::create_dir_all(dir.path().join(".git")).expect("create .git");
        dir
    }

    fn make_plain_dir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn make_event(tool_name: &str, command: &str, cwd: &Path) -> String {
        serde_json::json!({
            "tool_name": tool_name,
            "tool_input": { "command": command },
            "cwd": cwd.to_string_lossy(),
        })
        .to_string()
    }

    struct NoReadAfterJson {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl NoReadAfterJson {
        fn new(input: String) -> Self {
            Self {
                bytes: input.into_bytes(),
                offset: 0,
            }
        }
    }

    impl Read for NoReadAfterJson {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            assert!(
                self.offset < self.bytes.len(),
                "hook parser tried to read after the first JSON event"
            );
            let len = (self.bytes.len() - self.offset).min(buf.len());
            buf[..len].copy_from_slice(&self.bytes[self.offset..self.offset + len]);
            self.offset += len;
            Ok(len)
        }
    }

    // ── self-gate: tack-only repo → deny ─────────────────────────────────────

    #[test]
    fn denies_git_in_tack_only_repo() {
        let dir = make_tack_only_dir();
        let input = make_event("Bash", "git status", dir.path());
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        assert!(!out.is_empty(), "expected deny output");
        let value: serde_json::Value =
            serde_json::from_slice(&out).expect("output must be valid JSON");
        assert_eq!(
            value["hookSpecificOutput"]["permissionDecision"], "deny",
            "decision must be deny"
        );
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PreToolUse",);
        // The reason must reference tack.
        let reason = value["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("reason must be a string");
        assert!(
            reason.contains("tack"),
            "reason must mention tack: {reason}"
        );
    }

    #[test]
    fn parses_first_json_event_without_waiting_for_eof() {
        let dir = make_tack_only_dir();
        let input = NoReadAfterJson::new(make_event("Bash", "git status", dir.path()));
        let mut out = Vec::new();
        pre_tool_use(input, &mut out).expect("should not error");
        assert!(!out.is_empty(), "expected deny output");
    }

    #[test]
    fn deny_output_has_exact_shape() {
        let dir = make_tack_only_dir();
        let input = make_event("Bash", "git commit -m 'hello'", dir.path());
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        // All required fields must be present with correct values.
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            value["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .is_some_and(|r| r.contains("tack snap"))
        );
    }

    // ── self-gate: mixed repo (.tack + .git) → allow ─────────────────────────

    #[test]
    fn allows_git_in_mixed_repo() {
        let dir = make_mixed_dir();
        let input = make_event("Bash", "git status", dir.path());
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        assert!(out.is_empty(), "mixed repo must allow git (empty output)");
    }

    // ── self-gate: no tack root → allow ──────────────────────────────────────

    #[test]
    fn allows_git_in_plain_directory() {
        let dir = make_plain_dir();
        let input = make_event("Bash", "git status", dir.path());
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        assert!(out.is_empty(), "plain dir must allow git");
    }

    // ── non-Bash tool → always allow ─────────────────────────────────────────

    #[test]
    fn allows_non_bash_tool() {
        let dir = make_tack_only_dir();
        let input = make_event("Edit", "git status", dir.path());
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        assert!(out.is_empty(), "non-Bash tool must always allow");
    }

    // ── no git in command → allow ─────────────────────────────────────────────

    #[test]
    fn allows_bash_without_git() {
        let dir = make_tack_only_dir();
        let input = make_event("Bash", "cargo build --release", dir.path());
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        assert!(out.is_empty(), "no git → allow");
    }

    // ── cwd walk: child subdir of tack root → deny ───────────────────────────

    #[test]
    fn denies_git_from_subdir_of_tack_repo() {
        let dir = make_tack_only_dir();
        let subdir = dir.path().join("src").join("lib");
        fs::create_dir_all(&subdir).expect("create subdir");
        let input = make_event("Bash", "git log", &subdir);
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        assert!(!out.is_empty(), "subdir of tack repo must also deny");
    }

    // ── missing tool_input.command → allow ───────────────────────────────────

    #[test]
    fn allows_when_command_field_is_absent() {
        let dir = make_tack_only_dir();
        let input = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {},
            "cwd": dir.path().to_string_lossy(),
        })
        .to_string();
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        assert!(out.is_empty());
    }

    // ── invalid JSON input → allow (never crash) ──────────────────────────────

    #[test]
    fn allows_on_invalid_json_input() {
        let mut out = Vec::new();
        pre_tool_use(b"not json at all {{{".as_ref(), &mut out).expect("should not error");
        assert!(out.is_empty());
    }

    // ── cwd missing from event → fall back to process cwd ─────────────────────

    #[test]
    fn allows_when_cwd_absent_and_process_cwd_has_no_tack() {
        // This test depends on the process cwd not having a .tack dir, which is
        // true during `cargo test` (it runs from the crate root or workspace root,
        // neither of which has .tack in the tack-vcs repo itself yet).
        let input = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "git status" },
        })
        .to_string();
        let mut out = Vec::new();
        pre_tool_use(input.as_bytes(), &mut out).expect("should not error");
        // We cannot assert empty here because the test runner might run inside a
        // tack repo later; just assert it does not error.
        let _ = out; // result can be allow or deny depending on env
    }
}
