//! `tack` — the command-line client for `tack-core` (`DESIGN.md §12`).
//!
//! Every subcommand drives exactly one `tack-core` method, mirroring the
//! agent-native API (`DESIGN.md §11`): the CLI is one client of the engine SDK
//! (`constitution.md §5`). A global `--json` flag makes read commands emit the
//! same machine-readable [`Response`](tack_core::api::Response) shape an
//! out-of-process agent receives from `tack serve`.
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]

mod commands;
mod hook;
mod install;
mod mcp;

use std::io::Write;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// The output format for read commands.
///
/// `Human` is concise, readable text; `Json` emits one
/// [`Response`](tack_core::api::Response) JSON object, matching `tack serve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Concise, human-readable text (the default).
    Human,
    /// Machine-readable JSON for agents.
    Json,
}

impl OutputFormat {
    /// Picks the format from the global `--json` flag.
    const fn from_json_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }
}

/// `tack` — a content-addressed, agent-native version-control system.
#[derive(Debug, Parser)]
#[command(name = "tack", version, about, long_about = None)]
struct Cli {
    /// Emit machine-readable JSON (read commands).
    #[arg(long, global = true)]
    json: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// The top-level `tack` subcommands (`DESIGN.md §12`).
#[derive(Debug, Subcommand)]
enum Command {
    /// Create `.tack/` and the root op in the current directory.
    Init,
    /// Show the working directory against the current working-copy snapshot.
    Status,
    /// Create a named cut (close the working-copy snapshot, start a child).
    Snap {
        /// The cut message. Omitting it records a plain auto-snapshot instead.
        #[arg(short, long)]
        message: Option<String>,
        /// Capture ONLY these repo-relative paths into a scoped cut, taking
        /// everything else from the base cut (repeatable). Requires `-m`. Lets a
        /// worker checkpoint just its own files without folding in peers' edits.
        #[arg(long = "only")]
        only: Vec<String>,
        /// The base cut/op to overlay a scoped cut onto (default: the current
        /// base cut). Ignored unless `--only` is given.
        #[arg(long)]
        base: Option<String>,
    },
    /// Show the named-cut history, newest-first.
    Log {
        /// Show EVERY named cut across all lineages — including cuts left
        /// off-lineage by a restore — not just the current working copy's
        /// ancestry.
        #[arg(long)]
        all: bool,
    },
    /// Operation-log commands.
    Op {
        /// The `op` subcommand to run.
        #[command(subcommand)]
        command: OpCommand,
    },
    /// Show a diff; defaults to the live working copy (on-disk files) vs the last cut.
    Diff(DiffArgs),
    /// Non-destructively restore the working copy to an op or snapshot.
    Restore {
        /// The op or snapshot id (or unique prefix) to restore to.
        #[arg(long)]
        to: String,
    },
    /// Append an op reversing the most recent operation.
    Undo,
    /// Print an object's type tag and byte length.
    Cat {
        /// The object id (or unique prefix) to inspect.
        id: String,
    },
    /// List a tree's immediate entries (default: the working-copy root tree).
    Ls {
        /// The tree (or snapshot) id/prefix; defaults to the working-copy root.
        tree: Option<String>,
    },
    /// Print the agent-API schema: every method, its parameters, and its
    /// response shape (the same data the JSON `help` method returns).
    Schema,
    /// Show where the working copy currently is (op, base cut, heads, and
    /// whether the current state came from a restore).
    Current,
    /// List every named cut across all lineages (alias for `log --all`).
    Cuts,
    /// List op-derived team/release lanes.
    Lanes,
    /// Admit a cut to a team/release lane.
    Admit {
        /// The cut id or unique prefix to admit.
        cut: String,
        /// The target lane.
        #[arg(long = "to")]
        lane: String,
        /// Optional admission reason.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Backport a source cut to a release lane, or continue a settlement.
    Backport(BackportArgs),
    /// List the currently-held advisory claims.
    Claims,
    /// Record an advisory claim on a path (advisory only — never enforced).
    Claim {
        /// The repo-relative path to claim.
        path: String,
        /// The actor holding the claim (default: the author name).
        #[arg(long = "as")]
        holder: Option<String>,
        /// An optional advisory note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Release an advisory claim on a path.
    Release {
        /// The repo-relative path to release.
        path: String,
        /// The actor releasing the claim (default: the author name).
        #[arg(long = "as")]
        holder: Option<String>,
    },
    /// Run the JSON-RPC agent server over stdio.
    Serve,
    /// Continuously watch the working directory and auto-snapshot on change.
    Watch {
        /// The burst-coalescing window in milliseconds (default: 300).
        #[arg(long, default_value_t = 300)]
        debounce_ms: u64,
    },
    /// Project a snapshot tree as a lazy virtual filesystem (`ProjFS`).
    Mount {
        /// The directory to project into (created if missing).
        dir: String,
        /// The snapshot id/prefix to project; defaults to the current
        /// working-copy snapshot's root tree.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// Write per-repo AI-agent configuration (Claude Code and/or Codex CLI).
    Install {
        /// Configure Claude Code only (default: both).
        #[arg(long)]
        claude: bool,
        /// Configure Codex CLI only (default: both).
        #[arg(long)]
        codex: bool,
        /// Print planned changes without writing any files.
        #[arg(long)]
        dry_run: bool,
        /// Remove the managed instruction block and hook entries.
        #[arg(long)]
        uninstall: bool,
        /// Also register tack as an MCP server (writes .mcp.json for Claude;
        /// prints the `codex mcp add` command for Codex).
        #[arg(long)]
        with_mcp: bool,
    },
    /// Run as a Model Context Protocol server over stdio.
    Mcp,
    /// AI-agent hook handlers (hidden; invoked by the host agent, not users).
    #[command(hide = true)]
    Hook {
        /// The hook event subcommand.
        #[command(subcommand)]
        which: HookCmd,
    },
}

/// Nested subcommands under `tack op`.
#[derive(Debug, Subcommand)]
enum OpCommand {
    /// Show the operation log, newest-first.
    Log,
}

/// Nested subcommands under the hidden `tack hook`.
#[derive(Debug, Subcommand)]
enum HookCmd {
    /// Handle a Claude Code / Codex `PreToolUse` event from stdin.
    PreToolUse,
}

/// Arguments for `tack diff`.
#[derive(Debug, Args)]
struct DiffArgs {
    /// The "from" snapshot or op id/prefix (defaults to the working copy's last cut).
    #[arg(long)]
    from: Option<String>,
    /// The "to" snapshot or op id/prefix (defaults to the live working copy — current files on disk).
    #[arg(long)]
    to: Option<String>,
    /// Show per-file line-count stats instead of the file-level summary.
    #[arg(long)]
    stat: bool,
    /// Show per-file content hunks (a patch). Takes precedence over `--stat`.
    #[arg(long)]
    patch: bool,
}

/// Arguments for `tack backport`.
#[derive(Debug, Args)]
struct BackportArgs {
    /// The source fix cut id/prefix. Omit with `--continue`.
    source: Option<String>,
    /// The target lane.
    #[arg(long = "to")]
    target_lane: Option<String>,
    /// Optional target cut message.
    #[arg(short, long)]
    message: Option<String>,
    /// Optional backport reason.
    #[arg(long)]
    reason: Option<String>,
    /// Finish the current manual backport settlement.
    #[arg(long = "continue")]
    continue_settlement: bool,
    /// Also admit the resulting cut when one is created.
    #[arg(long)]
    admit: bool,
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    let format = OutputFormat::from_json_flag(cli.json);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    match run(&mut out, cli.command, format) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Print the full error chain to stderr, one cause per line, and exit
            // non-zero so callers (and CI) can detect failure.
            let mut stderr = std::io::stderr();
            let _ = writeln!(stderr, "error: {error}");
            for cause in error.chain().skip(1) {
                let _ = writeln!(stderr, "  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Initializes the tracing subscriber, honouring `RUST_LOG`.
///
/// Logs go to stderr so they never contaminate `--json` output on stdout.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Dispatches a parsed [`Command`] to its handler.
fn run(out: &mut impl Write, command: Command, format: OutputFormat) -> anyhow::Result<()> {
    match command {
        Command::Init => commands::init(out, format),
        Command::Status => commands::status(out, format),
        Command::Snap {
            message,
            only,
            base,
        } => commands::snap(out, format, message, only, base),
        Command::Log { all } => commands::log(out, format, all),
        Command::Op {
            command: OpCommand::Log,
        } => commands::op_log(out, format),
        Command::Diff(args) => {
            commands::diff(out, format, args.from, args.to, args.stat, args.patch)
        }
        Command::Restore { to } => commands::restore(out, format, to),
        Command::Undo => commands::undo(out, format),
        Command::Cat { id } => commands::cat(out, format, id),
        Command::Ls { tree } => commands::ls(out, format, tree),
        Command::Schema => commands::schema(out, format),
        Command::Current => commands::current(out, format),
        Command::Cuts => commands::cuts(out, format),
        Command::Lanes => commands::lanes(out, format),
        Command::Admit { cut, lane, reason } => commands::admit(out, format, cut, lane, reason),
        Command::Backport(args) => commands::backport(
            out,
            format,
            args.source,
            args.target_lane,
            args.message,
            args.reason,
            args.continue_settlement,
            args.admit,
        ),
        Command::Claims => commands::claims(out, format),
        Command::Claim { path, holder, note } => commands::claim(out, format, path, holder, note),
        Command::Release { path, holder } => commands::release(out, format, path, holder),
        Command::Serve => commands::serve(),
        Command::Watch { debounce_ms } => commands::watch(out, format, debounce_ms),
        Command::Mount { dir, snapshot } => commands::mount(out, format, dir, snapshot),
        Command::Install {
            claude,
            codex,
            dry_run,
            uninstall,
            with_mcp,
        } => commands::install(
            out,
            format,
            install::InstallOptions {
                claude,
                codex,
                dry_run,
                uninstall,
                with_mcp,
            },
        ),
        Command::Mcp => commands::mcp(),
        Command::Hook {
            which: HookCmd::PreToolUse,
        } => commands::hook_pre_tool_use(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_snap_with_message() {
        let cli = Cli::parse_from(["tack", "snap", "-m", "hello"]);
        assert!(matches!(
            cli.command,
            Command::Snap { message: Some(ref m), .. } if m == "hello"
        ));
    }

    #[test]
    fn parses_snap_scoped_only_and_base() {
        let cli = Cli::parse_from([
            "tack", "snap", "-m", "scoped", "--only", "src", "--only", "lib", "--base", "ab12",
        ]);
        let Command::Snap {
            message,
            only,
            base,
        } = cli.command
        else {
            panic!("expected snap");
        };
        assert_eq!(message.as_deref(), Some("scoped"));
        assert_eq!(only, vec!["src".to_owned(), "lib".to_owned()]);
        assert_eq!(base.as_deref(), Some("ab12"));
    }

    #[test]
    fn parses_log_all_flag() {
        let cli = Cli::parse_from(["tack", "log", "--all"]);
        assert!(matches!(cli.command, Command::Log { all: true }));
        let cli = Cli::parse_from(["tack", "log"]);
        assert!(matches!(cli.command, Command::Log { all: false }));
    }

    #[test]
    fn parses_diff_stat_and_patch() {
        let cli = Cli::parse_from(["tack", "diff", "--patch"]);
        let Command::Diff(args) = cli.command else {
            panic!("expected diff")
        };
        assert!(args.patch && !args.stat);
    }

    #[test]
    fn parses_claim_with_holder_and_note() {
        let cli = Cli::parse_from([
            "tack", "claim", "src/x.rs", "--as", "alice", "--note", "wip",
        ]);
        let Command::Claim { path, holder, note } = cli.command else {
            panic!("expected claim")
        };
        assert_eq!(path, "src/x.rs");
        assert_eq!(holder.as_deref(), Some("alice"));
        assert_eq!(note.as_deref(), Some("wip"));
    }

    #[test]
    fn parses_current_cuts_claims_schema() {
        assert!(matches!(
            Cli::parse_from(["tack", "current"]).command,
            Command::Current
        ));
        assert!(matches!(
            Cli::parse_from(["tack", "cuts"]).command,
            Command::Cuts
        ));
        assert!(matches!(
            Cli::parse_from(["tack", "claims"]).command,
            Command::Claims
        ));
        assert!(matches!(
            Cli::parse_from(["tack", "lanes"]).command,
            Command::Lanes
        ));
        assert!(matches!(
            Cli::parse_from(["tack", "schema"]).command,
            Command::Schema
        ));
    }

    #[test]
    fn parses_admit_and_backport() {
        let cli = Cli::parse_from([
            "tack",
            "admit",
            "deadbeef",
            "--to",
            "release/7.8.0",
            "--reason",
            "seed",
        ]);
        let Command::Admit { cut, lane, reason } = cli.command else {
            panic!("expected admit");
        };
        assert_eq!(cut, "deadbeef");
        assert_eq!(lane, "release/7.8.0");
        assert_eq!(reason.as_deref(), Some("seed"));

        let cli = Cli::parse_from([
            "tack",
            "backport",
            "abc123",
            "--to",
            "release/7.8.0",
            "-m",
            "hotfix",
            "--reason",
            "customer",
            "--admit",
        ]);
        let Command::Backport(args) = cli.command else {
            panic!("expected backport");
        };
        assert_eq!(args.source.as_deref(), Some("abc123"));
        assert_eq!(args.target_lane.as_deref(), Some("release/7.8.0"));
        assert_eq!(args.message.as_deref(), Some("hotfix"));
        assert_eq!(args.reason.as_deref(), Some("customer"));
        assert!(args.admit);
        assert!(!args.continue_settlement);
    }

    #[test]
    fn parses_backport_continue() {
        let cli = Cli::parse_from(["tack", "backport", "--continue", "--admit"]);
        let Command::Backport(args) = cli.command else {
            panic!("expected backport");
        };
        assert!(args.continue_settlement);
        assert!(args.admit);
        assert!(args.source.is_none());
    }

    #[test]
    fn parses_op_log_nested_subcommand() {
        let cli = Cli::parse_from(["tack", "op", "log"]);
        assert!(matches!(
            cli.command,
            Command::Op {
                command: OpCommand::Log
            }
        ));
    }

    #[test]
    fn json_flag_is_global_and_after_subcommand() {
        let cli = Cli::parse_from(["tack", "status", "--json"]);
        assert!(cli.json);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn parses_restore_to() {
        let cli = Cli::parse_from(["tack", "restore", "--to", "deadbeef"]);
        assert!(matches!(cli.command, Command::Restore { to } if to == "deadbeef"));
    }

    #[test]
    fn parses_watch_with_default_debounce() {
        let cli = Cli::parse_from(["tack", "watch"]);
        assert!(matches!(cli.command, Command::Watch { debounce_ms: 300 }));
    }

    #[test]
    fn parses_watch_with_custom_debounce() {
        let cli = Cli::parse_from(["tack", "watch", "--debounce-ms", "500"]);
        assert!(matches!(cli.command, Command::Watch { debounce_ms: 500 }));
    }

    #[test]
    fn parses_mount_with_default_snapshot() {
        let cli = Cli::parse_from(["tack", "mount", "view"]);
        assert!(matches!(
            cli.command,
            Command::Mount { ref dir, snapshot: None } if dir == "view"
        ));
    }

    #[test]
    fn parses_mount_with_snapshot() {
        let cli = Cli::parse_from(["tack", "mount", "view", "--snapshot", "deadbeef"]);
        let Command::Mount { dir, snapshot } = cli.command else {
            panic!("expected mount");
        };
        assert_eq!(dir, "view");
        assert_eq!(snapshot.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parses_diff_from_and_to() {
        let cli = Cli::parse_from(["tack", "diff", "--from", "aa", "--to", "bb"]);
        let Command::Diff(args) = cli.command else {
            panic!("expected diff");
        };
        assert_eq!(args.from.as_deref(), Some("aa"));
        assert_eq!(args.to.as_deref(), Some("bb"));
    }

    #[test]
    fn parses_mcp_subcommand() {
        let cli = Cli::parse_from(["tack", "mcp"]);
        assert!(matches!(cli.command, Command::Mcp));
    }

    #[test]
    fn parses_install_with_mcp_flag() {
        let cli = Cli::parse_from(["tack", "install", "--with-mcp"]);
        let Command::Install { with_mcp, .. } = cli.command else {
            panic!("expected install");
        };
        assert!(with_mcp, "--with-mcp flag must be set");
    }

    #[test]
    fn install_with_mcp_defaults_false_when_absent() {
        let cli = Cli::parse_from(["tack", "install"]);
        let Command::Install { with_mcp, .. } = cli.command else {
            panic!("expected install");
        };
        assert!(!with_mcp, "--with-mcp must default to false");
    }
}
