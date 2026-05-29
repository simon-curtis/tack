//! `tack install` — writes per-repo AI-agent configuration for Claude Code and
//! Codex CLI so they use `tack` instead of `git`.
//!
//! Three layers are written under the repo root:
//! 1. An instruction block in `CLAUDE.md` / `AGENTS.md` (soft guidance).
//! 2. A `PreToolUse` hook entry in `.claude/settings.json` / `.codex/hooks.json`
//!    (enforcement via the `tack hook pre-tool-use` handler).
//! 3. Optionally, a `tack.cmd` PATH shim on Windows when `tack` is not on PATH.
//!
//! All write operations are idempotent: running `tack install` twice leaves files
//! identical to a single run. Use `--uninstall` to reverse only managed changes.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use tack_core::Repository;

use crate::OutputFormat;

// ── instruction block markers ────────────────────────────────────────────────

const BLOCK_BEGIN: &str = "<!-- tack:begin (managed by `tack install`) -->";
const BLOCK_END: &str = "<!-- tack:end -->";

/// The managed instruction block content (same for Claude and Codex).
fn instruction_block_content() -> String {
    format!(
        "{BLOCK_BEGIN}\n\
         \n\
         ## This project uses tack, NOT git\n\
         \n\
         tack is a content-addressed, agent-native version-control system.\n\
         Do NOT run `git` commands — a `PreToolUse` hook will block them.\n\
         \n\
         ### Core tack commands\n\
         \n\
         | Goal | Command |\n\
         |---|---|\n\
         | Working-copy status | `tack status` |\n\
         | Where am I? | `tack current` |\n\
         | Active claims | `tack claims` |\n\
         | Save a named checkpoint | `tack snap -m \"your message\"` |\n\
         | Save specific files only | `tack snap -m \"msg\" --only path/to/file` |\n\
         | Diff live tree vs last cut | `tack diff` |\n\
         | Diff with content | `tack diff --patch` |\n\
         | Named-cut history | `tack log` |\n\
         | All cuts across lineages | `tack cuts` |\n\
         | Restore to a cut/op | `tack restore --to <id>` |\n\
         | Undo last operation | `tack undo` |\n\
         | Claim a file (advisory) | `tack claim <path>` |\n\
         | Release a claim | `tack release <path>` |\n\
         \n\
         A `PreToolUse` hook is installed: any `git` invocation will be blocked\n\
         with a reminder of the equivalent tack command.\n\
         \n\
         {BLOCK_END}"
    )
}

// ── PATH shim ─────────────────────────────────────────────────────────────────

/// Returns `true` if a bare `tack` executable is resolvable on `PATH`.
fn tack_on_path() -> bool {
    let Ok(path_var) = env::var("PATH") else {
        return false;
    };

    let candidates: &[&str] = if cfg!(windows) {
        &["tack.exe", "tack.cmd", "tack"]
    } else {
        &["tack"]
    };

    env::split_paths(&path_var).any(|dir| candidates.iter().any(|name| dir.join(name).is_file()))
}

/// Writes a `tack.cmd` shim to `%USERPROFILE%\.cargo\bin` on Windows.
///
/// Returns a human-readable summary of what was done (or what was skipped).
///
/// # Errors
///
/// Returns `Err` only on file-write failures; directory-not-found is treated as
/// a skip with a note.
fn ensure_path_shim(exe_path: &Path, dry_run: bool) -> Result<Option<String>> {
    if tack_on_path() {
        return Ok(None); // Already on PATH, nothing to do.
    }

    if cfg!(windows) {
        let cargo_bin = match env::var("USERPROFILE") {
            Ok(profile) => PathBuf::from(profile).join(".cargo").join("bin"),
            Err(_) => {
                return Ok(Some(
                    "note: USERPROFILE not set; add the tack exe directory to PATH manually"
                        .to_owned(),
                ));
            }
        };

        if !cargo_bin.is_dir() {
            return Ok(Some(format!(
                "note: {} does not exist; add {} to PATH manually",
                cargo_bin.display(),
                exe_path.parent().map_or_else(
                    || exe_path.display().to_string(),
                    |p| p.display().to_string()
                )
            )));
        }

        let shim_path = cargo_bin.join("tack.cmd");
        // ASCII, CRLF, no BOM.
        let content = format!("@echo off\r\n\"{}\" %*\r\n", exe_path.display());

        if dry_run {
            return Ok(Some(format!(
                "would write PATH shim: {}\n  contents: @echo off / \"{}\" %*",
                shim_path.display(),
                exe_path.display()
            )));
        }

        std::fs::write(&shim_path, content.as_bytes())
            .with_context(|| format!("failed to write shim to {}", shim_path.display()))?;

        return Ok(Some(format!("wrote PATH shim: {}", shim_path.display())));
    }

    // Non-Windows: advise the user.
    Ok(Some(format!(
        "note: tack not found on PATH — add {} to PATH",
        exe_path.parent().map_or_else(
            || exe_path.display().to_string(),
            |p| p.display().to_string()
        )
    )))
}

/// Removes the `tack.cmd` shim from `.cargo/bin` if it was written by us.
///
/// We identify "our" shim by checking that the file contains our exe path.
/// Returns a summary string.
fn remove_path_shim(exe_path: &Path, dry_run: bool) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }

    let cargo_bin = env::var("USERPROFILE")
        .ok()
        .map(|p| PathBuf::from(p).join(".cargo").join("bin"))?;

    let shim_path = cargo_bin.join("tack.cmd");
    if !shim_path.is_file() {
        return None;
    }

    let content = std::fs::read_to_string(&shim_path).ok()?;
    let exe_str = exe_path.display().to_string();
    if !content.contains(&exe_str) {
        return None; // Not our shim.
    }

    if dry_run {
        return Some(format!("would remove PATH shim: {}", shim_path.display()));
    }

    std::fs::remove_file(&shim_path).ok()?;
    Some(format!("removed PATH shim: {}", shim_path.display()))
}

// ── instruction block management ──────────────────────────────────────────────

/// Reads a file, or returns an empty string if it does not exist.
fn read_file_or_empty(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

/// Returns the number of bytes that constitute a line-ending at the start of
/// `s`: `2` for `\r\n`, `1` for `\n`, `0` otherwise.
const fn leading_newline_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'\r' && bytes[1] == b'\n' {
        2
    } else if !bytes.is_empty() && bytes[0] == b'\n' {
        1
    } else {
        0
    }
}

/// Inserts or replaces the managed instruction block in `text`.
///
/// Replaces between `BLOCK_BEGIN` and `BLOCK_END` markers if found; otherwise
/// appends the block at the end (with a leading newline for separation).
fn upsert_block(text: &str, block: &str) -> String {
    if let (Some(begin_pos), Some(end_pos)) = (text.find(BLOCK_BEGIN), text.find(BLOCK_END))
        && begin_pos < end_pos
    {
        let end_of_end = end_pos + BLOCK_END.len();
        // Consume the trailing newline after BLOCK_END if present.
        let after = text.get(end_of_end..).unwrap_or("");
        let skip = leading_newline_len(after);
        let mut result = text[..begin_pos].to_owned();
        result.push_str(block);
        result.push('\n');
        result.push_str(&text[end_of_end + skip..]);
        return result;
    }

    // No existing block — append.
    let mut result = text.to_owned();
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    if !result.is_empty() {
        result.push('\n');
    }
    result.push_str(block);
    result.push('\n');
    result
}

/// Removes the managed instruction block from `text`, including markers.
fn remove_block(text: &str) -> String {
    let Some(begin_pos) = text.find(BLOCK_BEGIN) else {
        return text.to_owned();
    };
    let Some(end_pos) = text.find(BLOCK_END) else {
        return text.to_owned();
    };
    if begin_pos >= end_pos {
        return text.to_owned();
    }

    let end_of_end = end_pos + BLOCK_END.len();
    let after = text.get(end_of_end..).unwrap_or("");
    let skip = leading_newline_len(after);

    // Trim a leading blank line before the block if present.
    let before = &text[..begin_pos];
    let trim = usize::from(before.ends_with("\n\n"));

    let mut result = text[..begin_pos - trim].to_owned();
    result.push_str(&text[end_of_end + skip..]);
    result
}

/// Writes the instruction block to `file_path`, creating parent dirs if needed.
///
/// Returns a human description of the change made.
///
/// # Errors
///
/// Fails on I/O errors.
fn write_instruction_block(file_path: &Path, agent_label: &str, dry_run: bool) -> Result<String> {
    let block = instruction_block_content();
    let existing = read_file_or_empty(file_path)?;
    let updated = upsert_block(&existing, &block);

    if updated == existing {
        return Ok(format!(
            "{agent_label}: {}: no change needed",
            file_path.display()
        ));
    }

    if dry_run {
        return Ok(format!(
            "{agent_label}: would write instruction block to {}",
            file_path.display()
        ));
    }

    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    std::fs::write(file_path, updated.as_bytes())
        .with_context(|| format!("failed to write {}", file_path.display()))?;

    Ok(format!(
        "{agent_label}: wrote instruction block to {}",
        file_path.display()
    ))
}

/// Removes the instruction block from `file_path`.
///
/// Returns a description of the change.
///
/// # Errors
///
/// Fails on I/O errors.
fn remove_instruction_block(file_path: &Path, agent_label: &str, dry_run: bool) -> Result<String> {
    if !file_path.exists() {
        return Ok(format!(
            "{agent_label}: {} not found, skipped",
            file_path.display()
        ));
    }

    let existing = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;
    let updated = remove_block(&existing);

    if updated == existing {
        return Ok(format!(
            "{agent_label}: no managed block found in {}",
            file_path.display()
        ));
    }

    if dry_run {
        return Ok(format!(
            "{agent_label}: would remove instruction block from {}",
            file_path.display()
        ));
    }

    std::fs::write(file_path, updated.as_bytes())
        .with_context(|| format!("failed to write {}", file_path.display()))?;

    Ok(format!(
        "{agent_label}: removed instruction block from {}",
        file_path.display()
    ))
}

// ── JSON hook config management ───────────────────────────────────────────────

/// Reads `path` as a JSON `Value`, returning `{}` if the file does not exist.
///
/// # Errors
///
/// Fails on I/O or parse errors.
fn read_json_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse JSON in {}", path.display()))
}

/// Returns the hook entry object for the given `matcher` and `exe_path`.
fn hook_entry(exe_path: &Path, matcher: &str) -> Value {
    serde_json::json!({
        "matcher": matcher,
        "hooks": [{
            "type": "command",
            "command": format!("\"{}\" hook pre-tool-use", exe_path.display()),
            "timeout": 10,
        }]
    })
}

/// The command string used to identify our hook entry (for deduplication).
fn our_hook_command(exe_path: &Path) -> String {
    format!("\"{}\" hook pre-tool-use", exe_path.display())
}

/// Merges our `PreToolUse` hook entry into `value` under `hooks.PreToolUse`.
///
/// Preserves all existing keys; deduplicates by the `command` field.
fn merge_hook_entry(value: &mut Value, exe_path: &Path, matcher: &str) -> bool {
    let hooks_obj = value
        .as_object_mut()
        .expect("value must be an object")
        .entry("hooks")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    let pre_tool_use_arr = hooks_obj
        .as_object_mut()
        .expect("hooks must be an object")
        .entry("PreToolUse")
        .or_insert_with(|| Value::Array(Vec::new()));

    let arr = pre_tool_use_arr
        .as_array_mut()
        .expect("PreToolUse must be an array");

    let our_command = our_hook_command(exe_path);

    // Check for an existing entry with the same command (dedup).
    let already_present = arr.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c == our_command)
                })
            })
    });

    if already_present {
        return false; // Nothing to add.
    }

    arr.push(hook_entry(exe_path, matcher));
    true
}

/// Removes our hook entry (matched by command string) from `value`.
///
/// Returns `true` if something was removed.
fn remove_hook_entry(value: &mut Value, exe_path: &Path) -> bool {
    let Some(hooks_obj) = value.as_object_mut().and_then(|o| o.get_mut("hooks")) else {
        return false;
    };
    let Some(arr) = hooks_obj
        .as_object_mut()
        .and_then(|o| o.get_mut("PreToolUse"))
        .and_then(|v| v.as_array_mut())
    else {
        return false;
    };

    let our_command = our_hook_command(exe_path);
    let before = arr.len();
    arr.retain(|entry| {
        !entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c == our_command)
                })
            })
    });
    arr.len() < before
}

/// Writes `value` as pretty-printed JSON to `path`, creating parent dirs.
///
/// # Errors
///
/// Fails on I/O errors.
fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("failed to serialise hook config")?;
    std::fs::write(path, text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Merges our hook entry into the JSON config at `config_path`.
///
/// Returns a description of the change.
///
/// # Errors
///
/// Fails on I/O or JSON parse errors.
fn write_hook_config(
    config_path: &Path,
    exe_path: &Path,
    matcher: &str,
    agent_label: &str,
    dry_run: bool,
) -> Result<String> {
    let mut value = read_json_or_empty(config_path)?;
    let changed = merge_hook_entry(&mut value, exe_path, matcher);

    if !changed {
        return Ok(format!(
            "{agent_label}: hook config {}: no change needed",
            config_path.display()
        ));
    }

    if dry_run {
        let preview = serde_json::to_string_pretty(&value).unwrap_or_default();
        return Ok(format!(
            "{agent_label}: would write hook config to {}:\n{}",
            config_path.display(),
            preview
        ));
    }

    write_json(config_path, &value)?;
    Ok(format!(
        "{agent_label}: wrote hook entry to {}",
        config_path.display()
    ))
}

/// Removes our hook entry from the JSON config at `config_path`.
///
/// Returns a description of the change.
///
/// # Errors
///
/// Fails on I/O or JSON parse errors.
fn remove_hook_config(
    config_path: &Path,
    exe_path: &Path,
    agent_label: &str,
    dry_run: bool,
) -> Result<String> {
    if !config_path.exists() {
        return Ok(format!(
            "{agent_label}: {} not found, skipped",
            config_path.display()
        ));
    }

    let mut value = read_json_or_empty(config_path)?;
    let changed = remove_hook_entry(&mut value, exe_path);

    if !changed {
        return Ok(format!(
            "{agent_label}: no managed hook found in {}",
            config_path.display()
        ));
    }

    if dry_run {
        return Ok(format!(
            "{agent_label}: would remove hook entry from {}",
            config_path.display()
        ));
    }

    write_json(config_path, &value)?;
    Ok(format!(
        "{agent_label}: removed hook entry from {}",
        config_path.display()
    ))
}

// ── targets ───────────────────────────────────────────────────────────────────

/// Which AI agents to configure.
#[derive(Debug, Clone, Copy)]
pub struct Targets {
    pub claude: bool,
    pub codex: bool,
}

/// Options passed from the `tack install` CLI subcommand.
///
/// Each field maps directly to a clap flag; bools are the natural representation
/// for boolean flags. The `struct_excessive_bools` lint is suppressed here because
/// this is a CLI argument struct, not a domain-logic type.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "direct mirror of clap boolean flags; no domain logic"
)]
pub struct InstallOptions {
    /// Configure Claude Code (default: both when neither flag is set).
    pub claude: bool,
    /// Configure Codex CLI (default: both when neither flag is set).
    pub codex: bool,
    /// Print planned changes without writing any files.
    pub dry_run: bool,
    /// Remove the managed instruction block and hook entries.
    pub uninstall: bool,
    /// Also register tack as an MCP server.
    pub with_mcp: bool,
}

impl Targets {
    /// Builds a `Targets` from the CLI flags; defaults to both when neither is
    /// specified.
    pub const fn from_flags(claude: bool, codex: bool) -> Self {
        if claude || codex {
            Self { claude, codex }
        } else {
            Self {
                claude: true,
                codex: true,
            }
        }
    }
}

// ── MCP registration (.mcp.json / codex hint) ────────────────────────────────

/// Reads `.mcp.json` at `path`, upserts the `"tack"` server entry, and writes
/// the result back.
///
/// The entry is: `{"mcpServers":{"tack":{"command":"<exe>","args":["mcp"]}}}`.
/// Preserves all pre-existing keys (other server entries, custom keys).
///
/// Returns a description of the change.
///
/// # Errors
///
/// Fails on I/O or JSON parse errors.
fn write_mcp_json(mcp_json_path: &Path, exe_path: &Path, dry_run: bool) -> Result<String> {
    let mut value = read_json_or_empty(mcp_json_path)?;

    let servers = value
        .as_object_mut()
        .expect("mcp.json root must be an object")
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    let tack_entry = serde_json::json!({
        "command": exe_path.display().to_string(),
        "args": ["mcp"],
    });

    let already_present = servers.get("tack").is_some_and(|v| {
        v.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c == exe_path.display().to_string())
    });

    if already_present {
        return Ok(format!(
            "Claude (MCP): {}: no change needed",
            mcp_json_path.display()
        ));
    }

    servers
        .as_object_mut()
        .expect("mcpServers must be an object")
        .insert("tack".to_owned(), tack_entry);

    if dry_run {
        let preview = serde_json::to_string_pretty(&value).unwrap_or_default();
        return Ok(format!(
            "Claude (MCP): would write {} with:\n{}",
            mcp_json_path.display(),
            preview
        ));
    }

    write_json(mcp_json_path, &value)?;
    Ok(format!(
        "Claude (MCP): wrote tack MCP entry to {}",
        mcp_json_path.display()
    ))
}

/// Removes the `"tack"` entry from `.mcp.json`.
///
/// Returns a description of the change.
///
/// # Errors
///
/// Fails on I/O or JSON parse errors.
fn remove_mcp_json(mcp_json_path: &Path, dry_run: bool) -> Result<String> {
    if !mcp_json_path.exists() {
        return Ok(format!(
            "Claude (MCP): {} not found, skipped",
            mcp_json_path.display()
        ));
    }

    let mut value = read_json_or_empty(mcp_json_path)?;

    let removed = value
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(Value::as_object_mut)
        .is_some_and(|servers| servers.remove("tack").is_some());

    if !removed {
        return Ok(format!(
            "Claude (MCP): no tack entry in {}",
            mcp_json_path.display()
        ));
    }

    if dry_run {
        return Ok(format!(
            "Claude (MCP): would remove tack entry from {}",
            mcp_json_path.display()
        ));
    }

    write_json(mcp_json_path, &value)?;
    Ok(format!(
        "Claude (MCP): removed tack entry from {}",
        mcp_json_path.display()
    ))
}

/// Returns the `codex mcp add` command line for the user to run.
fn codex_mcp_add_hint(exe_path: &Path) -> String {
    format!(
        "codex mcp add tack -- \"{}\" mcp\n\
         note: Codex MCP servers are configured globally via `codex mcp`.",
        exe_path.display()
    )
}

/// Returns the `codex mcp remove` hint for uninstall.
const fn codex_mcp_remove_hint() -> &'static str {
    "codex mcp remove tack\n\
     note: Codex MCP servers are configured globally via `codex mcp`."
}

// ── install / uninstall ────────────────────────────────────────────────────────

/// Runs `tack install`.
///
/// # Errors
///
/// Fails if the repository cannot be opened/initialized, the exe path cannot be
/// read, or any file write fails.
pub fn install(
    out: &mut impl Write,
    format: OutputFormat,
    targets: Targets,
    dry_run: bool,
    with_mcp: bool,
) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;

    // Resolve or init the repo.
    let repo = Repository::open(&cwd)
        .or_else(|_| Repository::init(&cwd))
        .context("failed to open or initialize a tack repository")?;
    let root = repo.work_dir().to_path_buf();

    let exe_path = env::current_exe().context("failed to resolve current executable path")?;

    let mut messages: Vec<String> = Vec::new();

    // PATH shim (best-effort; errors become notes, not failures).
    match ensure_path_shim(&exe_path, dry_run) {
        Ok(Some(note)) => messages.push(note),
        Ok(None) => {}
        Err(e) => messages.push(format!("note: PATH shim skipped: {e}")),
    }

    // Claude.
    if targets.claude {
        messages.push(write_instruction_block(
            &root.join("CLAUDE.md"),
            "Claude",
            dry_run,
        )?);
        messages.push(write_hook_config(
            &root.join(".claude").join("settings.json"),
            &exe_path,
            "Bash",
            "Claude",
            dry_run,
        )?);
        if with_mcp {
            messages.push(write_mcp_json(&root.join(".mcp.json"), &exe_path, dry_run)?);
        }
    }

    // Codex.
    if targets.codex {
        messages.push(write_instruction_block(
            &root.join("AGENTS.md"),
            "Codex",
            dry_run,
        )?);
        messages.push(write_hook_config(
            &root.join(".codex").join("hooks.json"),
            &exe_path,
            "^Bash$",
            "Codex",
            dry_run,
        )?);
        if with_mcp {
            messages.push(codex_mcp_add_hint(&exe_path));
        }
    }

    // Codex caveat.
    let caveat = "Codex requires trusting new hooks — run `/hooks` in Codex \
                  (or pass `--dangerously-bypass-hook-trust`) the first time.";

    match format {
        OutputFormat::Json => {
            let summary = serde_json::json!({
                "ok": true,
                "dry_run": dry_run,
                "changes": messages,
                "note": caveat,
            });
            let json = serde_json::to_string(&summary).context("failed to serialise output")?;
            writeln!(out, "{json}").context("failed to write output")?;
        }
        OutputFormat::Human => {
            for msg in &messages {
                writeln!(out, "{msg}").context("failed to write output")?;
            }
            writeln!(out, "\n{caveat}").context("failed to write output")?;
        }
    }

    Ok(())
}

/// Runs `tack install --uninstall`.
///
/// # Errors
///
/// Fails if the repository cannot be opened or any file write fails.
pub fn uninstall(
    out: &mut impl Write,
    format: OutputFormat,
    targets: Targets,
    dry_run: bool,
    with_mcp: bool,
) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let repo = Repository::open(&cwd).context("no tack repository found")?;
    let root = repo.work_dir().to_path_buf();

    let exe_path = env::current_exe().context("failed to resolve current executable path")?;

    let mut messages: Vec<String> = Vec::new();

    // Remove PATH shim (best-effort).
    if let Some(note) = remove_path_shim(&exe_path, dry_run) {
        messages.push(note);
    }

    if targets.claude {
        messages.push(remove_instruction_block(
            &root.join("CLAUDE.md"),
            "Claude",
            dry_run,
        )?);
        messages.push(remove_hook_config(
            &root.join(".claude").join("settings.json"),
            &exe_path,
            "Claude",
            dry_run,
        )?);
        if with_mcp {
            messages.push(remove_mcp_json(&root.join(".mcp.json"), dry_run)?);
        }
    }

    if targets.codex {
        messages.push(remove_instruction_block(
            &root.join("AGENTS.md"),
            "Codex",
            dry_run,
        )?);
        messages.push(remove_hook_config(
            &root.join(".codex").join("hooks.json"),
            &exe_path,
            "Codex",
            dry_run,
        )?);
        if with_mcp {
            messages.push(codex_mcp_remove_hint().to_owned());
        }
    }

    match format {
        OutputFormat::Json => {
            let summary = serde_json::json!({
                "ok": true,
                "dry_run": dry_run,
                "changes": messages,
            });
            let json = serde_json::to_string(&summary).context("failed to serialise output")?;
            writeln!(out, "{json}").context("failed to write output")?;
        }
        OutputFormat::Human => {
            for msg in &messages {
                writeln!(out, "{msg}").context("failed to write output")?;
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Creates a tempdir with a `.tack/config` so `Repository::open` succeeds.
    fn make_tack_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(dir.path()).expect("init repo");
        dir
    }

    fn fake_exe() -> PathBuf {
        PathBuf::from(r"C:\fake\tack.exe")
    }

    // ── upsert_block ──────────────────────────────────────────────────────────

    #[test]
    fn upsert_block_appends_to_empty_file() {
        let block = format!("{BLOCK_BEGIN}\ncontent\n{BLOCK_END}");
        let result = upsert_block("", &block);
        assert!(result.contains(BLOCK_BEGIN));
        assert!(result.contains(BLOCK_END));
    }

    #[test]
    fn upsert_block_appends_to_existing_content() {
        let existing = "# Existing docs\n\nsome text\n";
        let block = format!("{BLOCK_BEGIN}\ncontent\n{BLOCK_END}");
        let result = upsert_block(existing, &block);
        assert!(result.starts_with("# Existing docs"));
        assert!(result.contains(BLOCK_BEGIN));
    }

    #[test]
    fn upsert_block_replaces_existing_block() {
        let old_block = format!("{BLOCK_BEGIN}\nold content\n{BLOCK_END}");
        let existing = format!("# Header\n\n{old_block}\n\n# Footer\n");
        let new_block = format!("{BLOCK_BEGIN}\nnew content\n{BLOCK_END}");
        let result = upsert_block(&existing, &new_block);
        assert!(!result.contains("old content"), "old content must be gone");
        assert!(result.contains("new content"), "new content must appear");
        // Header and footer must survive.
        assert!(result.contains("# Header"));
        assert!(result.contains("# Footer"));
    }

    #[test]
    fn upsert_block_idempotent_on_identical_block() {
        let block = instruction_block_content();
        let first = upsert_block("", &block);
        let second = upsert_block(&first, &block);
        assert_eq!(first, second, "second upsert must be a no-op");
    }

    // ── remove_block ──────────────────────────────────────────────────────────

    #[test]
    fn remove_block_removes_managed_block() {
        let block = format!("{BLOCK_BEGIN}\ncontent\n{BLOCK_END}");
        let existing = format!("# Header\n\n{block}\n\n# Footer\n");
        let result = remove_block(&existing);
        assert!(!result.contains(BLOCK_BEGIN));
        assert!(!result.contains(BLOCK_END));
        assert!(result.contains("# Header"));
        assert!(result.contains("# Footer"));
    }

    #[test]
    fn remove_block_no_op_when_absent() {
        let text = "# No managed block here\n";
        assert_eq!(remove_block(text), text);
    }

    // ── merge_hook_entry ──────────────────────────────────────────────────────

    #[test]
    fn merge_hook_entry_adds_to_empty_value() {
        let exe = fake_exe();
        let mut value = Value::Object(serde_json::Map::new());
        let changed = merge_hook_entry(&mut value, &exe, "Bash");
        assert!(changed);
        let arr = value["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn merge_hook_entry_idempotent() {
        let exe = fake_exe();
        let mut value = Value::Object(serde_json::Map::new());
        merge_hook_entry(&mut value, &exe, "Bash");
        let changed = merge_hook_entry(&mut value, &exe, "Bash");
        assert!(!changed, "second merge must report no change");
        let arr = value["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(arr.len(), 1, "must not duplicate");
    }

    #[test]
    fn merge_hook_entry_preserves_unrelated_keys() {
        let exe = fake_exe();
        let mut value = serde_json::json!({ "customKey": "preserved", "other": 42 });
        merge_hook_entry(&mut value, &exe, "Bash");
        assert_eq!(value["customKey"], "preserved");
        assert_eq!(value["other"], 42);
    }

    #[test]
    fn merge_hook_entry_preserves_existing_hook_entries() {
        let exe = fake_exe();
        let mut value = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "SomeTool", "hooks": [{ "type": "command", "command": "other-tool" }] }
                ]
            }
        });
        merge_hook_entry(&mut value, &exe, "Bash");
        let arr = value["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(arr.len(), 2, "existing entry must be preserved");
        // Our entry is present.
        let our_cmd = our_hook_command(&exe);
        let has_ours = arr.iter().any(|e| {
            e.get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|h| h.iter().any(|h| h["command"] == our_cmd))
        });
        assert!(has_ours);
    }

    // ── remove_hook_entry ─────────────────────────────────────────────────────

    #[test]
    fn remove_hook_entry_removes_our_entry() {
        let exe = fake_exe();
        let mut value = Value::Object(serde_json::Map::new());
        merge_hook_entry(&mut value, &exe, "Bash");
        let removed = remove_hook_entry(&mut value, &exe);
        assert!(removed);
        let arr = value["hooks"]["PreToolUse"].as_array().expect("array");
        assert!(arr.is_empty());
    }

    #[test]
    fn remove_hook_entry_leaves_unrelated_entries() {
        let exe = fake_exe();
        let mut value = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Other", "hooks": [{ "type": "command", "command": "other" }] }
                ]
            }
        });
        merge_hook_entry(&mut value, &exe, "Bash");
        remove_hook_entry(&mut value, &exe);
        let arr = value["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["matcher"], "Other");
    }

    #[test]
    fn remove_hook_entry_no_op_when_absent() {
        let exe = fake_exe();
        let mut value = serde_json::json!({ "unrelated": true });
        let removed = remove_hook_entry(&mut value, &exe);
        assert!(!removed);
    }

    // ── file-level install tests ───────────────────────────────────────────────

    fn run_install(root: &Path, exe: &Path, targets: Targets, dry_run: bool) {
        // write_instruction_block / write_hook_config directly (bypasses repo lookup).
        if targets.claude {
            write_instruction_block(&root.join("CLAUDE.md"), "Claude", dry_run)
                .expect("write claude md");
            write_hook_config(
                &root.join(".claude").join("settings.json"),
                exe,
                "Bash",
                "Claude",
                dry_run,
            )
            .expect("write claude hooks");
        }
        if targets.codex {
            write_instruction_block(&root.join("AGENTS.md"), "Codex", dry_run)
                .expect("write agents md");
            write_hook_config(
                &root.join(".codex").join("hooks.json"),
                exe,
                "^Bash$",
                "Codex",
                dry_run,
            )
            .expect("write codex hooks");
        }
    }

    fn run_uninstall(root: &Path, exe: &Path, targets: Targets, dry_run: bool) {
        if targets.claude {
            remove_instruction_block(&root.join("CLAUDE.md"), "Claude", dry_run)
                .expect("remove claude md");
            remove_hook_config(
                &root.join(".claude").join("settings.json"),
                exe,
                "Claude",
                dry_run,
            )
            .expect("remove claude hooks");
        }
        if targets.codex {
            remove_instruction_block(&root.join("AGENTS.md"), "Codex", dry_run)
                .expect("remove agents md");
            remove_hook_config(
                &root.join(".codex").join("hooks.json"),
                exe,
                "Codex",
                dry_run,
            )
            .expect("remove codex hooks");
        }
    }

    #[test]
    fn install_creates_claude_md_and_settings() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(true, false);

        run_install(root, &exe, targets, false);

        assert!(root.join("CLAUDE.md").exists());
        assert!(root.join(".claude").join("settings.json").exists());

        let md = fs::read_to_string(root.join("CLAUDE.md")).expect("read CLAUDE.md");
        assert!(md.contains(BLOCK_BEGIN));
        assert!(md.contains("tack snap"));

        let json: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".claude").join("settings.json")).expect("read settings"),
        )
        .expect("parse settings");
        let arr = json["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn install_creates_agents_md_and_codex_hooks() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(false, true);

        run_install(root, &exe, targets, false);

        assert!(root.join("AGENTS.md").exists());
        assert!(root.join(".codex").join("hooks.json").exists());

        let json: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".codex").join("hooks.json")).expect("read hooks"),
        )
        .expect("parse hooks");
        let arr = json["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(arr[0]["matcher"], "^Bash$");
    }

    #[test]
    fn install_idempotent_no_duplicate_entries() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(true, true);

        run_install(root, &exe, targets, false);
        let md_first = fs::read_to_string(root.join("CLAUDE.md")).expect("md");
        let settings_first =
            fs::read_to_string(root.join(".claude").join("settings.json")).expect("settings");

        run_install(root, &exe, targets, false);
        let md_second = fs::read_to_string(root.join("CLAUDE.md")).expect("md 2");
        let settings_second =
            fs::read_to_string(root.join(".claude").join("settings.json")).expect("settings 2");

        assert_eq!(md_first, md_second, "CLAUDE.md must be identical on re-run");
        assert_eq!(
            settings_first, settings_second,
            "settings.json must be identical on re-run"
        );

        // Confirm no duplicate hook entries.
        let json: Value = serde_json::from_str(&settings_second).expect("parse");
        let arr = json["hooks"]["PreToolUse"].as_array().expect("array");
        assert_eq!(arr.len(), 1, "must have exactly one hook entry");
    }

    #[test]
    fn install_preserves_pre_existing_json_keys() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();

        // Write a pre-existing settings.json with an unrelated key.
        let settings_path = root.join(".claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &settings_path,
            r#"{"existingKey": "preserved", "anotherKey": 99}"#,
        )
        .expect("write pre-existing settings");

        let targets = Targets::from_flags(true, false);
        run_install(root, &exe, targets, false);

        let json: Value = serde_json::from_str(&fs::read_to_string(&settings_path).expect("read"))
            .expect("parse");
        assert_eq!(
            json["existingKey"], "preserved",
            "pre-existing key must survive"
        );
        assert_eq!(json["anotherKey"], 99, "numeric key must survive");
    }

    #[test]
    fn instruction_block_insert_then_replace_gives_one_block() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(true, false);

        // First install.
        run_install(root, &exe, targets, false);

        // Second install — must replace, not append.
        run_install(root, &exe, targets, false);

        let md = fs::read_to_string(root.join("CLAUDE.md")).expect("md");
        let begin_count = md.matches(BLOCK_BEGIN).count();
        let end_count = md.matches(BLOCK_END).count();
        assert_eq!(begin_count, 1, "exactly one begin marker");
        assert_eq!(end_count, 1, "exactly one end marker");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(true, true);

        run_install(root, &exe, targets, true);

        assert!(
            !root.join("CLAUDE.md").exists(),
            "dry-run must not create CLAUDE.md"
        );
        assert!(
            !root.join("AGENTS.md").exists(),
            "dry-run must not create AGENTS.md"
        );
        assert!(
            !root.join(".claude").join("settings.json").exists(),
            "dry-run must not create settings.json"
        );
        assert!(
            !root.join(".codex").join("hooks.json").exists(),
            "dry-run must not create hooks.json"
        );
    }

    #[test]
    fn uninstall_removes_block_and_hook_but_keeps_unrelated_keys() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(true, true);

        // Pre-existing content and key.
        let settings_path = root.join(".claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().expect("parent")).expect("mkdir");
        fs::write(&settings_path, r#"{"retainMe": true}"#).expect("write");

        let md_path = root.join("CLAUDE.md");
        fs::write(&md_path, "# Existing\n\nHello\n").expect("write md");

        run_install(root, &exe, targets, false);

        // Confirm installed.
        let md = fs::read_to_string(&md_path).expect("md after install");
        assert!(md.contains(BLOCK_BEGIN));
        let json: Value = serde_json::from_str(
            &fs::read_to_string(&settings_path).expect("settings after install"),
        )
        .expect("parse");
        assert!(
            json["hooks"]["PreToolUse"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        );

        // Uninstall.
        run_uninstall(root, &exe, targets, false);

        let md_after = fs::read_to_string(&md_path).expect("md after uninstall");
        assert!(
            !md_after.contains(BLOCK_BEGIN),
            "block must be removed after uninstall"
        );
        assert!(
            md_after.contains("# Existing"),
            "pre-existing content must survive"
        );

        let json_after: Value = serde_json::from_str(
            &fs::read_to_string(&settings_path).expect("settings after uninstall"),
        )
        .expect("parse");
        assert_eq!(
            json_after["retainMe"], true,
            "unrelated key must survive uninstall"
        );
        let arr = json_after["hooks"]["PreToolUse"].as_array().expect("array");
        assert!(arr.is_empty(), "hook entry must be removed");
    }

    #[test]
    fn dry_run_uninstall_writes_nothing() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(true, true);

        // First do a real install.
        run_install(root, &exe, targets, false);

        let md_before = fs::read_to_string(root.join("CLAUDE.md")).expect("md");
        let settings_before =
            fs::read_to_string(root.join(".claude").join("settings.json")).expect("settings");

        // Dry-run uninstall must change nothing on disk.
        run_uninstall(root, &exe, targets, true);

        let md_after = fs::read_to_string(root.join("CLAUDE.md")).expect("md");
        let settings_after =
            fs::read_to_string(root.join(".claude").join("settings.json")).expect("settings");

        assert_eq!(
            md_before, md_after,
            "dry-run uninstall must not change CLAUDE.md"
        );
        assert_eq!(
            settings_before, settings_after,
            "dry-run uninstall must not change settings.json"
        );
    }

    // ── Targets helper ────────────────────────────────────────────────────────

    #[test]
    fn targets_defaults_to_both_when_neither_flag_set() {
        let t = Targets::from_flags(false, false);
        assert!(t.claude && t.codex);
    }

    #[test]
    fn targets_respects_explicit_flags() {
        let t = Targets::from_flags(true, false);
        assert!(t.claude && !t.codex);
        let t = Targets::from_flags(false, true);
        assert!(!t.claude && t.codex);
    }

    // ── MCP install (write_mcp_json / remove_mcp_json) ───────────────────────

    #[test]
    fn with_mcp_writes_mcp_json_for_claude() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let targets = Targets::from_flags(true, false);

        run_install(root, &exe, targets, false);
        write_mcp_json(&root.join(".mcp.json"), &exe, false).expect("write .mcp.json");

        let mcp_path = root.join(".mcp.json");
        assert!(mcp_path.exists(), ".mcp.json must be created");

        let json: Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).expect("read .mcp.json"))
                .expect("parse .mcp.json");
        assert_eq!(
            json["mcpServers"]["tack"]["command"],
            exe.display().to_string(),
            "tack command must match the exe path"
        );
        assert_eq!(json["mcpServers"]["tack"]["args"][0], "mcp");
    }

    #[test]
    fn with_mcp_idempotent_two_runs_leave_one_entry() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let mcp_path = root.join(".mcp.json");

        write_mcp_json(&mcp_path, &exe, false).expect("first write");
        let first = fs::read_to_string(&mcp_path).expect("read after first");
        write_mcp_json(&mcp_path, &exe, false).expect("second write");
        let second = fs::read_to_string(&mcp_path).expect("read after second");

        assert_eq!(first, second, "second write must be idempotent");

        let json: Value = serde_json::from_str(&second).expect("parse");
        // Exactly one tack entry, not a duplicate.
        assert_eq!(
            json["mcpServers"]
                .as_object()
                .expect("object")
                .keys()
                .filter(|k| k.as_str() == "tack")
                .count(),
            1,
            "exactly one tack entry"
        );
    }

    #[test]
    fn with_mcp_preserves_pre_existing_server_keys() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let mcp_path = root.join(".mcp.json");

        // Write a pre-existing .mcp.json with another server.
        fs::write(
            &mcp_path,
            r#"{"mcpServers":{"other":{"command":"other-cmd","args":[]}}}"#,
        )
        .expect("pre-existing .mcp.json");

        write_mcp_json(&mcp_path, &exe, false).expect("write tack entry");

        let json: Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).expect("read")).expect("parse");
        // Both servers must be present.
        assert!(
            json["mcpServers"].get("other").is_some(),
            "pre-existing server must be preserved"
        );
        assert!(
            json["mcpServers"].get("tack").is_some(),
            "tack server must be added"
        );
    }

    #[test]
    fn with_mcp_dry_run_writes_nothing() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let mcp_path = root.join(".mcp.json");

        write_mcp_json(&mcp_path, &exe, true /* dry_run */).expect("dry-run write");
        assert!(!mcp_path.exists(), "dry-run must not create .mcp.json");
    }

    #[test]
    fn with_mcp_uninstall_removes_tack_entry_keeps_others() {
        let dir = make_tack_repo();
        let root = dir.path();
        let mcp_path = root.join(".mcp.json");

        // Set up: write with two servers (the tack entry uses a hardcoded path here;
        // remove_mcp_json matches by key name, not exe path).
        fs::write(
            &mcp_path,
            r#"{"mcpServers":{"other":{"command":"other-cmd","args":[]},"tack":{"command":"tack","args":["mcp"]}}}"#,
        )
        .expect("write pre-existing");

        remove_mcp_json(&mcp_path, false).expect("remove tack entry");

        let json: Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).expect("read")).expect("parse");
        assert!(
            json["mcpServers"].get("other").is_some(),
            "other server must survive uninstall"
        );
        assert!(
            json["mcpServers"].get("tack").is_none(),
            "tack entry must be removed"
        );
    }

    #[test]
    fn with_mcp_uninstall_dry_run_writes_nothing() {
        let dir = make_tack_repo();
        let root = dir.path();
        let exe = fake_exe();
        let mcp_path = root.join(".mcp.json");

        write_mcp_json(&mcp_path, &exe, false).expect("install");
        let before = fs::read_to_string(&mcp_path).expect("read before");

        remove_mcp_json(&mcp_path, true /* dry_run */).expect("dry-run uninstall");
        let after = fs::read_to_string(&mcp_path).expect("read after");

        assert_eq!(before, after, "dry-run uninstall must not change .mcp.json");
    }

    #[test]
    fn codex_mcp_add_hint_contains_correct_command() {
        let exe = fake_exe();
        let hint = codex_mcp_add_hint(&exe);
        assert!(
            hint.contains("codex mcp add tack"),
            "hint must contain 'codex mcp add tack', got: {hint}"
        );
        assert!(
            hint.contains("mcp"),
            "hint must reference the mcp subcommand, got: {hint}"
        );
    }

    #[test]
    fn codex_mcp_remove_hint_contains_correct_command() {
        let hint = codex_mcp_remove_hint();
        assert!(
            hint.contains("codex mcp remove tack"),
            "hint must contain 'codex mcp remove tack', got: {hint}"
        );
    }
}
