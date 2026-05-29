//! The continuous file-watch daemon (`DESIGN.md §7`, §12).
//!
//! This realizes "continuous snapshots": it watches the repository's working
//! directory recursively and, on relevant filesystem activity, auto-snapshots
//! by calling [`Repository::snapshot_working_copy`]. A short debounce coalesces
//! a burst of edits (a "save all", a checkout, a build step) into a single
//! snapshot rather than one per inotify/`ReadDirectoryChanges` callback.
//!
//! ## The feedback-loop guard
//!
//! [`Repository::snapshot_working_copy`] writes into `.tack/` (objects and the
//! `op-head` pointer). Those writes themselves fire watch events, so a naive
//! watcher would snapshot, observe its own writes, snapshot again, and spin
//! forever. [`should_snapshot`] is the guard: an event is only worth a snapshot
//! if at least one of its paths is **outside** `.tack/` and **not** ignored by
//! the repository's [`IgnoreRules`]. Events touching only `.tack/` (or only
//! ignored paths) are dropped, breaking the loop.
//!
//! ## Testability
//!
//! The decision logic ([`should_snapshot`]) and the burst-coalescing logic
//! ([`Debouncer`]) are pure and deterministic — they take an explicit clock /
//! explicit inputs — so they are unit-tested without real filesystem timing or
//! `sleep`s (`rust.md`: zero tolerance for flaky tests). Only [`watch`] itself
//! touches the OS, and it is driven by a caller-supplied shutdown flag.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use notify::{Event, RecursiveMode, Watcher, recommended_watcher};

use crate::error::{Error, Result};
use crate::ignore::IgnoreRules;
use crate::repo::Repository;

/// The default burst-coalescing window (`DESIGN.md §7`).
///
/// Filesystem events arriving within this window of one another are collapsed
/// into a single snapshot. ~300 ms is long enough to absorb an editor's
/// save-all or a multi-file checkout, yet short enough to feel immediate.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// How often the watch loop wakes to check the debounce timer and the shutdown
/// flag when no events are arriving.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Options controlling a [`watch`] run.
///
/// Construct with [`WatchOptions::default`] (the recommended debounce) and
/// override fields as needed via struct-update syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    /// The burst-coalescing window. Defaults to [`DEFAULT_DEBOUNCE`].
    pub debounce: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self { debounce: DEFAULT_DEBOUNCE }
    }
}

/// Returns `true` if a batch of event `paths` warrants an auto-snapshot.
///
/// This is the feedback-loop guard described in the module docs and the single
/// piece of decision logic the watch loop relies on. A batch is worth a
/// snapshot when **any** of its paths is a real working-tree change:
///
/// * not inside the `.tack/` control directory (those are our own writes), and
/// * not ignored by `ignore` (so editor swap files, build artifacts, etc.
///   listed in `.tackignore` never wake the daemon).
///
/// `paths` may be absolute (as the OS reports them) or already relative to the
/// work dir; both are handled by stripping the `tack_dir`'s parent prefix when
/// present. An empty batch is never worth a snapshot.
///
/// `tack_dir` is the absolute path to the repository's `.tack/` directory (i.e.
/// [`Repository::tack_dir`]).
pub fn should_snapshot<P: AsRef<Path>>(
    paths: &[P],
    tack_dir: &Path,
    ignore: &IgnoreRules,
) -> bool {
    // The work dir is the parent of `.tack/`; relative paths are taken against
    // it so `.tackignore` rules (which match repo-relative paths) apply.
    let work_dir = tack_dir.parent();
    paths
        .iter()
        .any(|path| is_relevant_change(path.as_ref(), tack_dir, work_dir, ignore))
}

/// Returns `true` if a single event `path` is a real working-tree change worth
/// snapshotting (outside `.tack/` and not ignored).
fn is_relevant_change(
    path: &Path,
    tack_dir: &Path,
    work_dir: Option<&Path>,
    ignore: &IgnoreRules,
) -> bool {
    // Our own writes into `.tack/` must never trigger a snapshot, or the daemon
    // would feed itself forever.
    if path.starts_with(tack_dir) {
        return false;
    }

    // Reduce to a repo-relative path for the ignore check. If the path is not
    // under the work dir (or the work dir is unknown), fall back to the path as
    // given — `IgnoreRules` normalizes separators and matches by basename for
    // slash-free rules, so a relative path still matches sensibly.
    let rel: &Path = work_dir
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);

    // A bare `.tack` component anywhere (e.g. a relative `.tack/...`) is always
    // ignored by `IgnoreRules`, which also covers the relative-path case.
    !ignore.is_ignored(rel, false)
}

/// Coalesces a stream of timestamped events into at-most-one fire per quiet
/// window — the pure core of the watch loop's debounce.
///
/// Feed observed events with [`record`](Self::record); ask whether the window
/// has gone quiet with [`due`](Self::due). Both take an explicit [`Instant`],
/// so the coalescing behaviour is fully deterministic and unit-testable without
/// real time passing.
///
/// The contract is *trailing-edge* debounce: after the last `record`, `due`
/// returns `true` exactly once, once `window` has elapsed with no further
/// `record`. Calling `due` consumes the pending state, so a subsequent call
/// returns `false` until the next `record`.
#[derive(Debug)]
pub struct Debouncer {
    /// The quiet window an edit burst must clear before it is `due`.
    window: Duration,
    /// The instant of the most recent recorded event, if a fire is pending.
    last_event: Option<Instant>,
}

impl Debouncer {
    /// Creates a debouncer with the given quiet `window`.
    pub const fn new(window: Duration) -> Self {
        Self { window, last_event: None }
    }

    /// Records that a relevant event was observed at `now`, (re)arming the
    /// trailing-edge timer.
    pub const fn record(&mut self, now: Instant) {
        self.last_event = Some(now);
    }

    /// Returns `true` if a burst is pending and the quiet `window` has elapsed
    /// since the last recorded event as of `now`.
    ///
    /// On firing it clears the pending state, so it returns `true` at most once
    /// per burst.
    pub fn due(&mut self, now: Instant) -> bool {
        let Some(last) = self.last_event else {
            return false;
        };
        if now.duration_since(last) >= self.window {
            self.last_event = None;
            return true;
        }
        false
    }

    /// Returns `true` if an edit burst is currently pending (recorded but not
    /// yet fired).
    pub const fn is_pending(&self) -> bool {
        self.last_event.is_some()
    }
}

/// Watches the repository's working directory and auto-snapshots on change
/// until `shutdown` is set (`DESIGN.md §7`, §12).
///
/// Runs the continuous-snapshot loop: a recursive [`notify`] watcher feeds
/// filesystem events through [`should_snapshot`] (the feedback-loop guard) into
/// a [`Debouncer`]; when an edit burst goes quiet for `opts.debounce`, the loop
/// calls [`Repository::snapshot_working_copy`] and logs the resulting
/// working-copy id (short) via `tracing`.
///
/// The function blocks the calling thread. Set the shared `shutdown` flag (for
/// example from a Ctrl-C handler) to make it return cleanly after the current
/// poll tick.
///
/// # Errors
///
/// * [`Error::Watch`] if the OS watcher cannot be created or fails to begin
///   watching the work dir.
/// * Store / [`Error::Io`] errors propagated from
///   [`Repository::snapshot_working_copy`] when a triggered snapshot fails.
pub fn watch(repo: &Repository, opts: &WatchOptions, shutdown: &Arc<AtomicBool>) -> Result<()> {
    // A bounded-by-nature std mpsc carries events from the watcher's callback
    // thread to this loop. The closure only forwards; all policy lives here so
    // it stays testable.
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<Event, notify::Error>>();
    let mut watcher = recommended_watcher(move |event| {
        // If the receiver is gone the loop has already exited; dropping the
        // event is correct.
        let _ = tx.send(event);
    })
    .map_err(|e| watch_err(&e))?;
    watcher
        .watch(repo.work_dir(), RecursiveMode::Recursive)
        .map_err(|e| watch_err(&e))?;

    let ignore = IgnoreRules::load(repo.work_dir().join(".tackignore"))?;
    let tack_dir = repo.tack_dir().to_path_buf();
    let mut debouncer = Debouncer::new(opts.debounce);

    while !shutdown.load(Ordering::Relaxed) {
        // Drain everything currently queued without blocking, so a burst is
        // coalesced into a single re-arm of the debouncer.
        drain_pending(&rx, &tack_dir, &ignore, &mut debouncer);

        // If a burst has gone quiet long enough, take one snapshot.
        if debouncer.due(Instant::now()) {
            take_snapshot(repo)?;
        }

        // Block briefly for the next event so we are not a busy-loop, but wake
        // often enough to honour the debounce timer and the shutdown flag.
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) => record_if_relevant(event, &tack_dir, &ignore, &mut debouncer),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // The sender was dropped (watcher gone) — nothing more can arrive.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // A burst that was still pending when shutdown arrived is flushed so no
    // observed change is silently lost.
    if debouncer.is_pending() {
        take_snapshot(repo)?;
    }
    Ok(())
}

/// Drains all immediately-available events from `rx`, re-arming `debouncer` for
/// each relevant batch. Non-blocking.
fn drain_pending(
    rx: &std::sync::mpsc::Receiver<std::result::Result<Event, notify::Error>>,
    tack_dir: &Path,
    ignore: &IgnoreRules,
    debouncer: &mut Debouncer,
) {
    while let Ok(event) = rx.try_recv() {
        record_if_relevant(event, tack_dir, ignore, debouncer);
    }
}

/// Feeds one watcher result to the debouncer iff it is a relevant change.
///
/// Watcher errors are non-fatal (a transient OS hiccup, an overflow): they are
/// logged and dropped rather than aborting the daemon.
fn record_if_relevant(
    event: std::result::Result<Event, notify::Error>,
    tack_dir: &Path,
    ignore: &IgnoreRules,
    debouncer: &mut Debouncer,
) {
    match event {
        Ok(event) => {
            if should_snapshot(&event.paths, tack_dir, ignore) {
                debouncer.record(Instant::now());
            }
        }
        Err(error) => {
            tracing::warn!(%error, "watch event error (ignored)");
        }
    }
}

/// Takes one auto-snapshot and logs the resulting working-copy id (short).
fn take_snapshot(repo: &Repository) -> Result<()> {
    let id = repo.snapshot_working_copy()?;
    tracing::info!(snapshot = %id.short(), "auto-snapshot");
    Ok(())
}

/// Wraps a [`notify::Error`] into the crate error type.
fn watch_err(error: &notify::Error) -> Error {
    Error::Watch(error.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A `.tack/` dir under a fake work root, mirroring [`Repository`]'s layout.
    fn tack_dir() -> PathBuf {
        PathBuf::from("/work/repo").join(".tack")
    }

    // ── should_snapshot: the feedback-loop guard ─────────────────────────────

    #[test]
    fn event_under_tack_dir_does_not_snapshot() {
        let tack = tack_dir();
        let paths = [tack.join("objects/aa/bb"), tack.join("op-head")];
        assert!(
            !should_snapshot(&paths, &tack, &IgnoreRules::empty()),
            "our own writes into .tack/ must never trigger a snapshot"
        );
    }

    #[test]
    fn event_on_tracked_file_snapshots() {
        let tack = tack_dir();
        let paths = [PathBuf::from("/work/repo/src/main.rs")];
        assert!(
            should_snapshot(&paths, &tack, &IgnoreRules::empty()),
            "a normal tracked-file change must trigger a snapshot"
        );
    }

    #[test]
    fn ignored_path_does_not_snapshot() {
        let tack = tack_dir();
        let ignore = IgnoreRules::parse("*.log\ntarget/\n");
        let paths = [PathBuf::from("/work/repo/build/output.log")];
        assert!(
            !should_snapshot(&paths, &tack, &ignore),
            "a path matched by .tackignore must not trigger a snapshot"
        );
    }

    #[test]
    fn mixed_batch_snapshots_when_any_path_is_relevant() {
        let tack = tack_dir();
        let ignore = IgnoreRules::parse("*.log\n");
        // Two noise paths (our own .tack write + an ignored log) plus one real
        // source edit: the real edit must win.
        let paths = [
            tack.join("op-head"),
            PathBuf::from("/work/repo/debug.log"),
            PathBuf::from("/work/repo/src/lib.rs"),
        ];
        assert!(
            should_snapshot(&paths, &tack, &ignore),
            "a batch with any relevant path must trigger a snapshot"
        );
    }

    #[test]
    fn batch_of_only_noise_does_not_snapshot() {
        let tack = tack_dir();
        let ignore = IgnoreRules::parse("*.log\n");
        let paths = [tack.join("objects/aa/bb"), PathBuf::from("/work/repo/a.log")];
        assert!(
            !should_snapshot(&paths, &tack, &ignore),
            "a batch of only .tack/ + ignored paths must be dropped"
        );
    }

    #[test]
    fn empty_batch_does_not_snapshot() {
        let tack = tack_dir();
        let paths: [PathBuf; 0] = [];
        assert!(!should_snapshot(&paths, &tack, &IgnoreRules::empty()));
    }

    #[test]
    fn relative_tack_path_does_not_snapshot() {
        // Some backends may report paths relative to the watched root.
        let tack = tack_dir();
        let paths = [PathBuf::from(".tack/op-head")];
        assert!(
            !should_snapshot(&paths, &tack, &IgnoreRules::empty()),
            "a relative .tack/ path must also be filtered (IgnoreRules guards it)"
        );
    }

    #[test]
    fn relative_tracked_path_snapshots() {
        let tack = tack_dir();
        let paths = [PathBuf::from("src/main.rs")];
        assert!(should_snapshot(&paths, &tack, &IgnoreRules::empty()));
    }

    // ── Debouncer: deterministic coalescing (no real time) ───────────────────

    #[test]
    fn debouncer_not_due_before_window_elapses() {
        let start = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(300));
        d.record(start);
        assert!(!d.due(start + Duration::from_millis(100)), "still within the window");
        assert!(!d.due(start + Duration::from_millis(299)), "1 ms short of the window");
    }

    #[test]
    fn debouncer_fires_once_after_quiet_window() {
        let start = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(300));
        d.record(start);
        assert!(d.due(start + Duration::from_millis(300)), "exactly at the window fires");
        assert!(
            !d.due(start + Duration::from_millis(600)),
            "the fire is consumed: a second due() without a new event is false"
        );
    }

    #[test]
    fn debouncer_coalesces_a_burst_into_one_fire() {
        let start = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(300));
        // A burst of three events, each within the window of the previous, must
        // re-arm the timer and collapse into a single fire after the LAST one.
        d.record(start);
        d.record(start + Duration::from_millis(100));
        d.record(start + Duration::from_millis(200));
        // 300 ms after the first event, but only 100 ms after the last → not due.
        assert!(!d.due(start + Duration::from_millis(300)), "burst still in flight");
        // 300 ms after the LAST event → exactly one fire.
        assert!(d.due(start + Duration::from_millis(500)), "burst settles to one fire");
        assert!(!d.due(start + Duration::from_millis(900)), "no second fire");
    }

    #[test]
    fn debouncer_idle_is_never_due() {
        let mut d = Debouncer::new(Duration::from_millis(300));
        assert!(!d.due(Instant::now()), "no recorded event → never due");
        assert!(!d.is_pending());
    }

    #[test]
    fn debouncer_rearms_after_firing() {
        let start = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(300));
        d.record(start);
        assert!(d.due(start + Duration::from_millis(300)));
        // A fresh event after a fire arms a new window.
        let later = start + Duration::from_secs(1);
        d.record(later);
        assert!(d.is_pending());
        assert!(!d.due(later + Duration::from_millis(100)));
        assert!(d.due(later + Duration::from_millis(300)));
    }

    #[test]
    fn default_options_use_default_debounce() {
        assert_eq!(WatchOptions::default().debounce, DEFAULT_DEBOUNCE);
    }
}
