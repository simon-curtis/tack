//! Subcommand implementations for the `tack` CLI.
//!
//! Each command opens (or initializes) a [`Repository`] and drives exactly one
//! engine method, mirroring the agent-native API (`DESIGN.md §11`, §12). Read
//! commands honour a global `--json` flag: in JSON mode they emit the same
//! [`Response`](tack_core::api::Response) shape an out-of-process agent would
//! receive from `tack serve`, so humans and agents observe identical payloads.
//!
//! ## `snap` semantics (`DESIGN.md §12`)
//!
//! `tack snap` is a **named cut** (the analog of a commit). A message is the
//! point of a cut, so when `-m/--message` is omitted there is nothing to name:
//! rather than mint an empty-message cut that the human-facing `tack log` would
//! immediately hide (the engine's [`log`](Repository::log) skips empty-message
//! snapshots), this falls back to a plain auto-snapshot
//! ([`snapshot_working_copy`](Repository::snapshot_working_copy)). With `-m`, it
//! closes a named cut via [`named_cut`](Repository::named_cut).

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use tack_core::api::{self, Request, Response};
use tack_core::{Repository, WatchOptions};

use crate::OutputFormat;
use crate::install::Targets;

/// Holds the resolved author identity for a named cut.
///
/// Names and e-mails come from `TACK_AUTHOR_NAME` / `TACK_AUTHOR_EMAIL`, falling
/// back to host-derived defaults so no personal data is ever hard-coded
/// (`constitution.md §6`).
struct Author {
    name: String,
    email: String,
}

impl Author {
    /// Resolves the author identity from the environment.
    ///
    /// * name  ← `TACK_AUTHOR_NAME`,  else `USERNAME`/`USER`, else `"tack"`.
    /// * email ← `TACK_AUTHOR_EMAIL`, else `name@hostname` derived from
    ///   `COMPUTERNAME`/`HOSTNAME`, else `"tack@localhost"`.
    fn from_env() -> Self {
        let name = env_first(&["TACK_AUTHOR_NAME", "USERNAME", "USER"])
            .unwrap_or_else(|| "tack".to_owned());
        let email = env_first(&["TACK_AUTHOR_EMAIL"]).unwrap_or_else(|| {
            let host =
                env_first(&["COMPUTERNAME", "HOSTNAME"]).unwrap_or_else(|| "localhost".to_owned());
            format!("{name}@{host}")
        });
        Self { name, email }
    }
}

/// Returns the first non-empty value among the named environment variables.
fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
}

/// Opens the repository by discovering `.tack/` upward from the current
/// directory.
fn open_repo() -> Result<Repository> {
    let cwd = std::env::current_dir().context("failed to read the current directory")?;
    Repository::open(&cwd)
        .with_context(|| format!("no tack repository found at or above {}", cwd.display()))
}

/// Serializes `response` as a single JSON line to `out`.
fn emit_json(out: &mut impl Write, response: &Response) -> Result<()> {
    let json = serde_json::to_string(response).context("failed to serialize JSON response")?;
    writeln!(out, "{json}").context("failed to write output")
}

// ── init ────────────────────────────────────────────────────────────────────

/// `tack init` — create `.tack/` and the root op in the current directory.
///
/// `init` is intentionally *not* an agent-API method (you need a repository
/// before you can serve one), so it is handled directly here rather than through
/// [`api::handle`].
///
/// # Errors
///
/// Fails if the current directory cannot be read or the repository cannot be
/// initialized (e.g. one already exists here).
pub fn init(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the current directory")?;
    Repository::init(&cwd).with_context(|| {
        format!(
            "failed to initialize a tack repository at {}",
            cwd.display()
        )
    })?;
    match format {
        OutputFormat::Json => emit_json(out, &Response::Ok),
        OutputFormat::Human => writeln!(
            out,
            "initialized empty tack repository in {}",
            cwd.display()
        )
        .context("failed to write output"),
    }
}

// ── status ──────────────────────────────────────────────────────────────────

/// `tack status` — report the working directory against the working-copy
/// snapshot.
///
/// # Errors
///
/// Fails if the repository cannot be opened or status cannot be computed.
pub fn status(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Status);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    let Response::Status { data } = &response else {
        return render_error(out, &response);
    };
    if data.clean {
        writeln!(out, "working copy is clean").context("failed to write output")?;
        return Ok(());
    }
    for path in &data.added {
        writeln!(out, "A  {path}").context("failed to write output")?;
    }
    for path in &data.modified {
        writeln!(out, "M  {path}").context("failed to write output")?;
    }
    for path in &data.deleted {
        writeln!(out, "D  {path}").context("failed to write output")?;
    }
    Ok(())
}

// ── snap ────────────────────────────────────────────────────────────────────

/// `tack snap [-m MSG]` — create a named cut, or auto-snapshot when no message
/// is given (see the module docs for the rationale).
///
/// # Errors
///
/// Fails if the repository cannot be opened or the cut/snapshot cannot be
/// written.
pub fn snap(
    out: &mut impl Write,
    format: OutputFormat,
    message: Option<String>,
    only: Vec<String>,
    base: Option<String>,
) -> Result<()> {
    let repo = open_repo()?;

    // `--only` selects a scoped cut, which must be named (a scoped checkpoint
    // without a message has nothing to identify it).
    if !only.is_empty() {
        let Some(message) = message else {
            return Err(anyhow::anyhow!(
                "`tack snap --only <path>` requires a message (-m/--message)"
            ));
        };
        let author = Author::from_env();
        let response = api::handle(
            &repo,
            Request::ScopedCut {
                paths: only,
                message,
                author_name: author.name,
                author_email: author.email,
                base,
            },
        );
        if format == OutputFormat::Json {
            return emit_json(out, &response);
        }
        return match &response {
            Response::ScopedCut { data } => {
                writeln!(
                    out,
                    "scoped cut {} ({} captured, {} uncaptured outside scope)",
                    short(&data.cut),
                    data.captured.len(),
                    data.outside_changes.len()
                )
                .context("failed to write output")?;
                for path in &data.captured {
                    writeln!(out, "  captured {path}").context("failed to write output")?;
                }
                for path in &data.outside_changes {
                    writeln!(out, "  outside  {path}").context("failed to write output")?;
                }
                Ok(())
            }
            other => render_error(out, other),
        };
    }

    let response = message.map_or_else(
        // No message → nothing to name; fall back to a plain auto-snapshot.
        || api::handle(&repo, Request::Snapshot),
        |message| {
            let author = Author::from_env();
            api::handle(
                &repo,
                Request::NamedCut {
                    message,
                    author_name: author.name,
                    author_email: author.email,
                },
            )
        },
    );
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::NamedCut { cut, .. } => {
            writeln!(out, "created cut {}", short(cut)).context("failed to write output")
        }
        Response::Snapshot { snapshot, .. } => {
            writeln!(out, "snapshot {}", short(snapshot)).context("failed to write output")
        }
        other => render_error(out, other),
    }
}

// ── log ─────────────────────────────────────────────────────────────────────

/// `tack log` — print the named-cut history, newest-first.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the log cannot be read.
pub fn log(out: &mut impl Write, format: OutputFormat, all: bool) -> Result<()> {
    let repo = open_repo()?;
    let request = if all { Request::Cuts } else { Request::Log };
    let response = api::handle(&repo, request);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    // `log` and `cuts` both carry a `cuts` field of the same shape.
    let cuts = match &response {
        Response::Log { cuts } | Response::Cuts { cuts } => cuts,
        other => return render_error(out, other),
    };
    if cuts.is_empty() {
        writeln!(out, "no named cuts yet").context("failed to write output")?;
        return Ok(());
    }
    for cut in cuts {
        writeln!(
            out,
            "{} {} ({}, {})",
            short(&cut.id),
            cut.message,
            cut.author_name,
            cut.timestamp
        )
        .context("failed to write output")?;
    }
    Ok(())
}

/// `tack cuts` — list every named cut across all lineages (alias for
/// `tack log --all`).
///
/// # Errors
///
/// Fails if the repository cannot be opened or the cuts cannot be read.
pub fn cuts(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    log(out, format, true)
}

// ── lanes / admit / backport ────────────────────────────────────────────────

/// `tack lanes` — list op-derived team/release lanes.
///
/// # Errors
///
/// Fails if the repository cannot be opened or lanes cannot be read.
pub fn lanes(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Lanes);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    let Response::Lanes { lanes } = &response else {
        return render_error(out, &response);
    };
    if lanes.is_empty() {
        writeln!(out, "no lanes").context("failed to write output")?;
        return Ok(());
    }
    for lane in lanes {
        writeln!(
            out,
            "{} {} (admitted by {})",
            lane.name,
            short(&lane.cut),
            short(&lane.admission)
        )
        .context("failed to write output")?;
    }
    Ok(())
}

/// `tack admit <cut> --to <lane>` — admit a cut to a lane.
///
/// # Errors
///
/// Fails if the repository cannot be opened, the cut cannot be resolved, or the
/// admission cannot be recorded.
pub fn admit(
    out: &mut impl Write,
    format: OutputFormat,
    cut: String,
    lane: String,
    reason: Option<String>,
) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(
        &repo,
        Request::Admit {
            cut,
            lane,
            reason: reason.unwrap_or_default(),
        },
    );
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::Admitted { data } => writeln!(
            out,
            "admitted {} to {} (op {})",
            short(&data.cut),
            data.lane,
            short(&data.op)
        )
        .context("failed to write output"),
        other => render_error(out, other),
    }
}

/// `tack backport <source> --to <lane>` or `tack backport --continue`.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the backport cannot be created
/// or continued.
#[allow(clippy::too_many_arguments)]
pub fn backport(
    out: &mut impl Write,
    format: OutputFormat,
    source: Option<String>,
    target_lane: Option<String>,
    message: Option<String>,
    reason: Option<String>,
    continue_settlement: bool,
    admit: bool,
) -> Result<()> {
    let repo = open_repo()?;
    let author = Author::from_env();
    let request = if continue_settlement {
        if source.is_some() || target_lane.is_some() || message.is_some() || reason.is_some() {
            return Err(anyhow::anyhow!(
                "`tack backport --continue` does not take source, --to, -m, or --reason"
            ));
        }
        Request::BackportContinue {
            author_name: author.name,
            author_email: author.email,
            admit,
        }
    } else {
        let source =
            source.ok_or_else(|| anyhow::anyhow!("`tack backport` requires a source cut"))?;
        let target_lane =
            target_lane.ok_or_else(|| anyhow::anyhow!("`tack backport` requires --to <lane>"))?;
        Request::Backport {
            source,
            target_lane,
            message,
            reason: reason.unwrap_or_default(),
            author_name: author.name,
            author_email: author.email,
            admit,
        }
    };
    let response = api::handle(&repo, request);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::Backport { data } => render_backport(out, data),
        other => render_error(out, other),
    }
}

fn render_backport(out: &mut impl Write, data: &api::BackportData) -> Result<()> {
    match data.outcome.as_str() {
        "created" => {
            let cut = data.cut.as_deref().unwrap_or("<none>");
            writeln!(
                out,
                "backport {} to {} created {} ({})",
                short(&data.provenance.source_cut),
                data.provenance.target_lane,
                short(cut),
                data.method
            )
            .context("failed to write output")?;
        }
        "already_ported" => {
            let cut = data.cut.as_deref().unwrap_or("<none>");
            writeln!(
                out,
                "already backported {} to {} as {}",
                short(&data.provenance.source_cut),
                data.provenance.target_lane,
                short(cut)
            )
            .context("failed to write output")?;
        }
        "settlement" => {
            writeln!(
                out,
                "backport {} to {} needs settlement",
                short(&data.provenance.source_cut),
                data.provenance.target_lane
            )
            .context("failed to write output")?;
            for conflict in &data.conflicts {
                writeln!(out, "  conflict {conflict}").context("failed to write output")?;
            }
            writeln!(
                out,
                "resolve the working copy, then run `tack backport --continue`"
            )
            .context("failed to write output")?;
        }
        other => {
            writeln!(out, "backport outcome {other}").context("failed to write output")?;
        }
    }
    if let Some(admission) = &data.admission {
        writeln!(
            out,
            "admitted to {} (op {})",
            admission.lane,
            short(&admission.op)
        )
        .context("failed to write output")?;
    }
    Ok(())
}

// ── op log ──────────────────────────────────────────────────────────────────

/// `tack op log` — print the operation log, newest-first.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the op log cannot be read.
pub fn op_log(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::OpLog);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    let Response::OpLog { ops } = &response else {
        return render_error(out, &response);
    };
    for op in ops {
        writeln!(
            out,
            "{} {} ({})",
            short(&op.id),
            op.description,
            op.timestamp
        )
        .context("failed to write output")?;
    }
    Ok(())
}

// ── diff ────────────────────────────────────────────────────────────────────

/// `tack diff [--from A] [--to B]` — print a file-level diff.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the diff cannot be computed.
pub fn diff(
    out: &mut impl Write,
    format: OutputFormat,
    from: Option<String>,
    to: Option<String>,
    stat: bool,
    patch: bool,
) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(
        &repo,
        Request::Diff {
            from,
            to,
            stat,
            patch,
        },
    );
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::Diff { data, .. } => {
            for path in &data.added {
                writeln!(out, "+ {path}").context("failed to write output")?;
            }
            for path in &data.removed {
                writeln!(out, "- {path}").context("failed to write output")?;
            }
            for path in &data.modified {
                writeln!(out, "~ {path}").context("failed to write output")?;
            }
            Ok(())
        }
        Response::DiffStat {
            files,
            total_added,
            total_removed,
            ..
        } => {
            for file in files {
                if file.binary {
                    writeln!(out, "    bin       {}", file.path)
                        .context("failed to write output")?;
                } else {
                    writeln!(
                        out,
                        "  +{:<5} -{:<5} {}",
                        file.added_lines, file.removed_lines, file.path
                    )
                    .context("failed to write output")?;
                }
            }
            writeln!(out, "total +{total_added} -{total_removed}").context("failed to write output")
        }
        Response::DiffPatch { files, .. } => {
            for file in files {
                writeln!(out, "--- {} ({})", file.path, file.change)
                    .context("failed to write output")?;
                if file.binary {
                    writeln!(out, "binary files differ").context("failed to write output")?;
                    continue;
                }
                for hunk in &file.hunks {
                    writeln!(
                        out,
                        "@@ -{},{} +{},{} @@",
                        hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
                    )
                    .context("failed to write output")?;
                    for line in &hunk.lines {
                        let prefix = match line.tag.as_str() {
                            "insert" => '+',
                            "delete" => '-',
                            _ => ' ',
                        };
                        writeln!(out, "{prefix}{}", line.content)
                            .context("failed to write output")?;
                    }
                }
            }
            Ok(())
        }
        other => render_error(out, other),
    }
}

// ── restore ───────────────────────────────────────────────────────────────────

/// `tack restore --to <id>` — non-destructively restore the working copy.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the target cannot be restored.
pub fn restore(out: &mut impl Write, format: OutputFormat, target: String) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Restore { target });
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::Restored {
            op,
            restored_cut,
            previous_op,
            ..
        } => {
            let to = restored_cut.as_ref().map_or_else(
                || "an unnamed snapshot".to_string(),
                |cut| format!("{} {}", short(&cut.id), cut.message),
            );
            writeln!(out, "restored to {to}").context("failed to write output")?;
            writeln!(out, "  new op {}", short(op)).context("failed to write output")?;
            if let Some(prev) = previous_op {
                writeln!(out, "  was at {} — run `tack undo` to return", short(prev))
                    .context("failed to write output")?;
            }
            Ok(())
        }
        other => render_error(out, other),
    }
}

// ── undo ──────────────────────────────────────────────────────────────────────

/// `tack undo` — append an op reversing the most recent operation.
///
/// # Errors
///
/// Fails if the repository cannot be opened or there is nothing to undo.
pub fn undo(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Undo);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::Undone {
            op,
            undone_op,
            working_copy,
            ..
        } => {
            writeln!(out, "undone (new op {})", short(op)).context("failed to write output")?;
            if let Some(undone) = undone_op {
                writeln!(out, "  reversed op {}", short(undone))
                    .context("failed to write output")?;
            }
            writeln!(out, "  working copy now {}", short(working_copy))
                .context("failed to write output")?;
            Ok(())
        }
        other => render_error(out, other),
    }
}

// ── cat ───────────────────────────────────────────────────────────────────────

/// `tack cat <id>` — print an object's type tag and byte length.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the object cannot be read.
pub fn cat(out: &mut impl Write, format: OutputFormat, id: String) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Cat { id });
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::Cat { object } => writeln!(
            out,
            "{} {} {} bytes",
            short(&object.id),
            object.kind,
            object.size
        )
        .context("failed to write output"),
        other => render_error(out, other),
    }
}

// ── ls ────────────────────────────────────────────────────────────────────────

/// `tack ls [tree-id]` — list a tree's immediate entries (default: the
/// working-copy root tree).
///
/// # Errors
///
/// Fails if the repository cannot be opened or the tree cannot be listed.
pub fn ls(out: &mut impl Write, format: OutputFormat, tree: Option<String>) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Ls { tree });
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    let Response::Ls { entries } = &response else {
        return render_error(out, &response);
    };
    for entry in entries {
        writeln!(out, "{:<8} {} {}", entry.kind, short(&entry.id), entry.name)
            .context("failed to write output")?;
    }
    Ok(())
}

// ── schema ──────────────────────────────────────────────────────────────────

/// `tack schema` — print the agent-API self-description (the JSON `help`
/// method). Works without an open repository.
///
/// # Errors
///
/// Fails only if writing the output fails.
pub fn schema(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let methods = api::schema_methods();
    if format == OutputFormat::Json {
        return emit_json(out, &Response::Help { methods });
    }
    for method in &methods {
        writeln!(out, "{:<11} {}", method.method, method.summary)
            .context("failed to write output")?;
        for param in &method.params {
            let req = if param.required {
                "required"
            } else {
                "optional"
            };
            writeln!(
                out,
                "    {:<14} {:<9} {} [{req}]",
                param.name, param.ty, param.description
            )
            .context("failed to write output")?;
        }
        writeln!(out, "    -> {}", method.returns).context("failed to write output")?;
    }
    Ok(())
}

// ── current ─────────────────────────────────────────────────────────────────

/// `tack current` — report where the working copy currently is (op, base cut,
/// heads, and whether the current state came from a restore).
///
/// # Errors
///
/// Fails if the repository cannot be opened or the state cannot be read.
pub fn current(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Current);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    let Response::Current { data } = &response else {
        return render_error(out, &response);
    };
    writeln!(out, "op       {} {}", short(&data.op), data.operation)
        .context("failed to write output")?;
    let state = if data.clean { "clean" } else { "dirty" };
    writeln!(out, "working  {} ({state})", short(&data.working_copy))
        .context("failed to write output")?;
    match &data.base_cut {
        Some(cut) => writeln!(out, "base cut {} {}", short(&cut.id), cut.message)
            .context("failed to write output")?,
        None => writeln!(out, "base cut (none)").context("failed to write output")?,
    }
    if data.from_restore {
        writeln!(
            out,
            "note     current state came from a restore (`tack undo` to step back)"
        )
        .context("failed to write output")?;
    }
    Ok(())
}

// ── claims / claim / release ──────────────────────────────────────────────────

/// `tack claims` — list the currently-held advisory claims.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the claims cannot be read.
pub fn claims(out: &mut impl Write, format: OutputFormat) -> Result<()> {
    let repo = open_repo()?;
    let response = api::handle(&repo, Request::Claims);
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    let Response::Claims { claims } = &response else {
        return render_error(out, &response);
    };
    if claims.is_empty() {
        writeln!(out, "no active claims").context("failed to write output")?;
        return Ok(());
    }
    for claim in claims {
        let note = if claim.note.is_empty() {
            String::new()
        } else {
            format!(" — {}", claim.note)
        };
        writeln!(out, "{} [{}]{note}", claim.path, claim.holder)
            .context("failed to write output")?;
    }
    Ok(())
}

/// `tack claim <path> [--as <holder>] [--note <note>]` — record an advisory
/// claim. The holder defaults to the author name.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the claim cannot be recorded.
pub fn claim(
    out: &mut impl Write,
    format: OutputFormat,
    path: String,
    holder: Option<String>,
    note: Option<String>,
) -> Result<()> {
    let repo = open_repo()?;
    let holder = holder.unwrap_or_else(|| Author::from_env().name);
    let note = note.unwrap_or_default();
    let response = api::handle(&repo, Request::Claim { path, holder, note });
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    match &response {
        Response::Claimed {
            claim, conflicts, ..
        } => {
            writeln!(out, "claimed {} [{}]", claim.path, claim.holder)
                .context("failed to write output")?;
            for conflict in conflicts {
                writeln!(
                    out,
                    "  conflict: {} also held by {}",
                    conflict.path, conflict.holder
                )
                .context("failed to write output")?;
            }
            Ok(())
        }
        other => render_error(out, other),
    }
}

/// `tack release <path> [--as <holder>]` — release an advisory claim. The
/// holder defaults to the author name.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the release cannot be recorded.
pub fn release(
    out: &mut impl Write,
    format: OutputFormat,
    path: String,
    holder: Option<String>,
) -> Result<()> {
    let repo = open_repo()?;
    let holder = holder.unwrap_or_else(|| Author::from_env().name);
    let response = api::handle(&repo, Request::Release { path, holder });
    if format == OutputFormat::Json {
        return emit_json(out, &response);
    }
    let Response::Claims { claims } = &response else {
        return render_error(out, &response);
    };
    writeln!(out, "released; {} claim(s) remaining", claims.len()).context("failed to write output")
}

// ── serve ─────────────────────────────────────────────────────────────────────

/// `tack serve` — run the line-delimited JSON-RPC agent server over stdio.
///
/// # Errors
///
/// Fails if the repository cannot be opened or the serve loop hits an I/O error.
pub fn serve() -> Result<()> {
    let repo = open_repo()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    api::serve(&repo, stdin.lock(), &mut stdout.lock()).context("agent serve loop failed")
}

// ── watch ───────────────────────────────────────────────────────────────────

/// `tack watch [--debounce-ms N]` — continuously snapshot on filesystem change
/// (`DESIGN.md §7`, §12).
///
/// Opens the repository and runs the continuous-snapshot daemon
/// ([`tack_core::watch`]) on a worker thread, coalescing bursts of edits within
/// the debounce window into one snapshot. Each auto-snapshot is logged via
/// `tracing` (the new working-copy id, short).
///
/// Shutdown is graceful and crate-free: a shared shutdown flag is set when the
/// main thread observes end-of-input on stdin (Ctrl-Z then Enter on Windows,
/// Ctrl-D on Unix) — and on a hard Ctrl-C the OS terminates the process. The
/// daemon flushes any pending burst before returning.
///
/// # Errors
///
/// Fails if the repository cannot be opened, the watcher cannot start, or a
/// triggered snapshot fails.
pub fn watch(out: &mut impl Write, format: OutputFormat, debounce_ms: u64) -> Result<()> {
    let repo = open_repo()?;
    let opts = WatchOptions {
        debounce: Duration::from_millis(debounce_ms),
    };

    let work_dir = repo.work_dir().display().to_string();
    if format == OutputFormat::Json {
        emit_json(out, &Response::Ok)?;
    } else {
        writeln!(
            out,
            "watching {work_dir} (read EOF on stdin or Ctrl-C to stop)"
        )
        .context("failed to write output")?;
    }
    out.flush().context("failed to flush output")?;

    let shutdown = Arc::new(AtomicBool::new(false));

    // Run the blocking watch loop on a worker thread so the main thread can
    // wait for an end-of-input signal and request a graceful shutdown. The
    // repository is reopened inside the thread (it is not `Send`-shared) so the
    // closure owns everything it touches.
    let cwd = std::env::current_dir().context("failed to read the current directory")?;
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || -> Result<()> {
        let repo = Repository::open(&cwd)
            .with_context(|| format!("failed to open repository at {}", cwd.display()))?;
        tack_core::watch(&repo, &opts, &thread_shutdown).context("watch loop failed")
    });

    // Block until stdin reaches end-of-input, then signal shutdown. Discarding
    // the read bytes is intentional — any input is treated as "carry on", EOF
    // as "stop".
    let mut sink = String::new();
    let stdin = std::io::stdin();
    while !shutdown.load(Ordering::Relaxed) {
        sink.clear();
        match stdin.read_line(&mut sink) {
            Ok(0) | Err(_) => break, // EOF (or a read error) → stop.
            Ok(_) => {}              // Any other input → keep watching.
        }
    }
    shutdown.store(true, Ordering::Relaxed);

    // Join the worker and surface any error it produced.
    handle
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("watch thread panicked")))
}

// ── mount ─────────────────────────────────────────────────────────────────────

/// `tack mount <dir> [--snapshot <id>]` — project a snapshot tree as a lazy
/// virtual filesystem via Windows `ProjFS` (`DESIGN.md §10`, §12).
///
/// Resolves the tree to project — the `--snapshot` id/prefix's root tree, or the
/// current working-copy snapshot's root tree by default — then mounts it at
/// `dir`. The command is **always present**: in a build without the `projfs`
/// feature (or off Windows) the engine's [`mount`](tack_core::mount) returns a
/// typed [`ProjfsUnavailable`](tack_core::Error::ProjfsUnavailable) error
/// carrying the rebuild/enable instructions, surfaced here. With the feature it
/// mounts and blocks until end-of-input (Ctrl-Z then Enter on Windows / Ctrl-D),
/// or a hard Ctrl-C, then unmounts cleanly by dropping the projection guard.
///
/// # Errors
///
/// Fails if the repository cannot be opened, the snapshot cannot be resolved, or
/// the projection cannot be mounted (including when `ProjFS` is unavailable).
pub fn mount(
    out: &mut impl Write,
    format: OutputFormat,
    dir: String,
    snapshot: Option<String>,
) -> Result<()> {
    let repo = open_repo()?;

    // Resolve the root tree to project: the named snapshot's, or the current
    // working copy's by default.
    let root_tree = match snapshot {
        Some(id) => {
            let snap_id = repo
                .resolve_prefix(&id)
                .with_context(|| format!("could not resolve snapshot {id:?}"))?;
            repo.store()
                .get_snapshot(&snap_id)
                .with_context(|| format!("{id:?} is not a snapshot"))?
                .root_tree()
        }
        None => repo
            .working_copy()
            .context("failed to read the working copy")?
            .root_tree(),
    };

    let root = std::path::PathBuf::from(dir);
    // Clone the store handle (a path) so the projection owns its own — the
    // ProjFS runtime requires a 'static source.
    let store = repo.store().clone();

    run_mount(out, format, &root, store, root_tree)
}

/// Mounts the projection and blocks until shutdown (real `ProjFS` build).
#[cfg(all(windows, feature = "projfs"))]
fn run_mount(
    out: &mut impl Write,
    format: OutputFormat,
    root: &std::path::Path,
    store: tack_core::ObjectStore,
    root_tree: tack_core::ObjectId,
) -> Result<()> {
    // The returned guard keeps virtualization alive; dropping it unmounts.
    let _projection = tack_core::mount(root, store, root_tree)
        .with_context(|| format!("failed to mount projfs projection at {}", root.display()))?;

    if format == OutputFormat::Json {
        emit_json(out, &Response::Ok)?;
    } else {
        writeln!(
            out,
            "mounted {} (read EOF on stdin or Ctrl-C to unmount)",
            root.display()
        )
        .context("failed to write output")?;
    }
    out.flush().context("failed to flush output")?;

    // Block until stdin reaches end-of-input (graceful unmount). A hard Ctrl-C
    // terminates the process and the OS tears down virtualization.
    let stdin = std::io::stdin();
    let mut sink = String::new();
    loop {
        sink.clear();
        match stdin.read_line(&mut sink) {
            Ok(0) | Err(_) => break, // EOF (or read error) → unmount.
            Ok(_) => {}              // Any other input → stay mounted.
        }
    }

    // `_projection` drops here, stopping virtualization and draining callbacks.
    Ok(())
}

/// Stub for builds without the `projfs` feature (or off Windows): the engine
/// `mount` returns a typed error which is surfaced unchanged.
#[cfg(not(all(windows, feature = "projfs")))]
fn run_mount(
    _out: &mut impl Write,
    _format: OutputFormat,
    root: &std::path::Path,
    store: tack_core::ObjectStore,
    root_tree: tack_core::ObjectId,
) -> Result<()> {
    tack_core::mount(root, store, root_tree)
        .with_context(|| format!("failed to mount projfs projection at {}", root.display()))?;
    Ok(())
}

// ── install ────────────────────────────────────────────────────────────────────

/// `tack install [--claude] [--codex] [--dry-run] [--uninstall] [--with-mcp]` —
/// writes per-repo AI-agent configuration for Claude Code and/or Codex CLI.
///
/// # Errors
///
/// Fails if the repository cannot be opened/initialized or any file write fails.
pub fn install(
    out: &mut impl Write,
    format: OutputFormat,
    opts: crate::install::InstallOptions,
) -> Result<()> {
    let targets = Targets::from_flags(opts.claude, opts.codex);
    if opts.uninstall {
        crate::install::uninstall(out, format, targets, opts.dry_run, opts.with_mcp)
    } else {
        crate::install::install(out, format, targets, opts.dry_run, opts.with_mcp)
    }
}

// ── mcp ───────────────────────────────────────────────────────────────────────

/// `tack mcp` — run as a Model Context Protocol server over stdio.
///
/// Reads newline-delimited JSON-RPC 2.0 from stdin and writes responses to
/// stdout. All tracing/logging goes to stderr. EOF on stdin is the shutdown
/// signal (exit 0).
///
/// # Errors
///
/// Fails on fatal stdin/stdout I/O errors.
pub fn mcp() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    crate::mcp::serve_mcp(stdin.lock(), &mut stdout.lock()).context("MCP server loop failed")
}

// ── hook pre-tool-use ─────────────────────────────────────────────────────────

/// `tack hook pre-tool-use` — reads a `PreToolUse` JSON event on stdin and writes
/// a deny decision to stdout when `git` is run inside a tack-only repo.
///
/// Returns `Ok(())` always; a hook must not crash the host agent.
///
/// # Errors
///
/// Fails only on stdout write errors.
pub fn hook_pre_tool_use() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    crate::hook::pre_tool_use(stdin.lock(), &mut stdout.lock())
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Returns the first 12 characters of a full hex id for human-facing output.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// Surfaces a [`Response::Error`] (or any unexpected variant) as an `anyhow`
/// error so the process exits non-zero with a clear message.
fn render_error(_out: &mut impl Write, response: &Response) -> Result<()> {
    match response {
        Response::Error { message } => Err(anyhow::anyhow!(message.clone())),
        other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
    }
}
