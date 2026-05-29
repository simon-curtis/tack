//! `.tackignore` matching (`DESIGN.md §8`).
//!
//! Rules are gitignore-style: one glob per line, blank lines and `#` comments
//! are skipped, and a leading `!` negates an earlier match. Patterns match
//! **repo-relative** paths (forward-slash separated, no leading slash). The
//! `.tack/` control directory is **always** ignored, regardless of the rules.
//!
//! ## Pattern syntax
//!
//! A small, self-contained glob engine (the format is part of the user-facing
//! contract, not the on-disk format, so it carries no `FORMAT_VERSION` weight):
//!
//! | Token | Meaning |
//! |---|---|
//! | `*` | any run of characters **except** `/` |
//! | `**` | any run of characters **including** `/` (spanning directories) |
//! | `?` | any single character except `/` |
//! | leading `/` | anchor the pattern to the repo root |
//! | trailing `/` | the pattern matches directories only |
//! | other chars | matched literally |
//!
//! A pattern that contains no `/` (after stripping any trailing one) matches by
//! **basename** at any depth, exactly like gitignore (`*.log` ignores
//! `a/b/c.log`). A pattern that contains a `/` is matched against the whole
//! repo-relative path (anchored if it had a leading `/`, otherwise free to
//! match from the root — gitignore treats an embedded slash as rooting the
//! pattern).
//!
//! Last match wins: if several rules match a path, the final one in file order
//! decides, so a later `!pattern` can re-include something an earlier rule
//! excluded.

use std::path::Path;

use crate::error::Result;

/// The control directory that is always ignored.
const TACK_DIR: &str = ".tack";

/// One parsed ignore rule.
#[derive(Debug, Clone)]
struct Rule {
    /// The glob with any leading `!`, leading `/`, and trailing `/` stripped.
    glob: String,
    /// `true` if the rule began with `!` (re-includes a previously-ignored path).
    negated: bool,
    /// `true` if the pattern was anchored to the repo root with a leading `/`,
    /// or contains an interior `/` (gitignore roots such patterns).
    anchored: bool,
    /// `true` if a trailing `/` restricted the rule to directories only.
    dir_only: bool,
}

/// A compiled set of `.tackignore` rules.
///
/// Build one from text with [`IgnoreRules::parse`], from a file with
/// [`IgnoreRules::load`], or an empty set with [`IgnoreRules::empty`]. Query
/// with [`IgnoreRules::is_ignored`].
#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    /// Returns an empty rule set (matches nothing except the always-ignored
    /// `.tack/`).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parses ignore rules from the text of a `.tackignore` file.
    ///
    /// Blank lines and lines whose first non-whitespace character is `#` are
    /// skipped. Trailing whitespace on a line is trimmed (a literal trailing
    /// space can be preserved with a `\` escape, matching gitignore — but v0
    /// keeps it simple and just trims). Leading `!` negates; leading `/`
    /// anchors; trailing `/` restricts to directories.
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for raw in text.lines() {
            let line = raw.trim_end();
            // Skip blanks and comments. A leading '#' is a comment; a literal
            // '#' can be escaped as "\#".
            let trimmed_start = line.trim_start();
            if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
                continue;
            }

            let mut pattern = line;
            let negated = pattern.starts_with('!');
            if negated {
                pattern = &pattern[1..];
            }
            // An escaped leading '#' or '!' becomes literal.
            let pattern = pattern.strip_prefix('\\').unwrap_or(pattern);

            let dir_only = pattern.ends_with('/');
            let pattern = pattern.strip_suffix('/').unwrap_or(pattern);

            let had_leading_slash = pattern.starts_with('/');
            let pattern = pattern.strip_prefix('/').unwrap_or(pattern);

            if pattern.is_empty() {
                continue;
            }

            // gitignore: a pattern with a slash anywhere is anchored to the root.
            let anchored = had_leading_slash || pattern.contains('/');

            rules.push(Rule {
                glob: pattern.to_owned(),
                negated,
                anchored,
                dir_only,
            });
        }
        Self { rules }
    }

    /// Loads ignore rules from a `.tackignore` file at `path`.
    ///
    /// A missing file yields an empty rule set (only `.tack/` is ignored), which
    /// is the common case for a repo without a `.tackignore`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`](crate::Error::Io) for read failures other than the
    /// file simply not existing.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        match std::fs::read_to_string(path.as_ref()) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns `true` if the repo-relative `rel_path` should be ignored.
    ///
    /// `rel_path` is interpreted with either separator (`/` or `\\`); it is
    /// normalized to forward slashes internally. `is_dir` indicates whether the
    /// path names a directory (so directory-only `foo/` rules apply correctly).
    ///
    /// `.tack` (and anything beneath it) is always ignored. Otherwise the rules
    /// are evaluated in order and the **last** matching rule decides; a negated
    /// rule re-includes the path.
    pub fn is_ignored(&self, rel_path: impl AsRef<Path>, is_dir: bool) -> bool {
        let normalized = normalize(rel_path.as_ref());

        // The control directory is never tracked.
        if normalized == TACK_DIR || normalized.starts_with(".tack/") {
            return true;
        }

        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            if rule.matches(&normalized, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

impl Rule {
    /// Returns `true` if this rule's glob matches the normalized path.
    fn matches(&self, path: &str, _is_dir: bool) -> bool {
        if self.anchored {
            glob_match(&self.glob, path)
        } else {
            // Unanchored, slash-free patterns match by basename at any depth.
            let base = path.rsplit('/').next().unwrap_or(path);
            glob_match(&self.glob, base)
        }
    }
}

/// Normalizes a path to a forward-slash, leading/trailing-slash-free string.
fn normalize(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = s.replace('\\', "/");
    s.trim_matches('/').to_owned()
}

/// Matches `pattern` against `text` using the glob syntax documented on
/// [`IgnoreRules`]: `*` (no slash), `**` (any), `?` (one non-slash), literals.
///
/// Implemented as a backtracking matcher over byte slices. `**` is handled by
/// recognizing the `**` token (optionally with surrounding `/`) and letting it
/// consume any span including separators; a lone `*` never crosses a `/`.
fn glob_match(pattern: &str, text: &str) -> bool {
    match_segments(pattern.as_bytes(), text.as_bytes())
}

/// Backtracking glob matcher. `p` is the remaining pattern, `t` the remaining
/// text.
#[expect(
    clippy::similar_names,
    reason = "star_pi/star_ti and dstar_pi/dstar_ti are the standard pattern-index / \
              text-index backtrack pairs of a glob matcher; the `pi`/`ti` suffixes are \
              clearer here than any contrived rename"
)]
fn match_segments(p: &[u8], t: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    // Saved state for the most recent `*` (single-segment) backtrack point.
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;
    // Saved state for the most recent `**` (cross-segment) backtrack point.
    let mut dstar_pi: Option<usize> = None;
    let mut dstar_ti = 0usize;

    while ti <= t.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' if pi + 1 < p.len() && p[pi + 1] == b'*' => {
                    // `**` — matches across separators. Skip the token plus an
                    // optional following '/'.
                    let mut next = pi + 2;
                    if next < p.len() && p[next] == b'/' {
                        next += 1;
                    }
                    dstar_pi = Some(next);
                    dstar_ti = ti;
                    // Also clear the single-star point: `**` supersedes it here.
                    star_pi = None;
                    pi = next;
                    continue;
                }
                b'*' => {
                    // Single `*` — matches any run NOT containing '/'.
                    star_pi = Some(pi);
                    star_ti = ti;
                    pi += 1;
                    continue;
                }
                b'?' if ti < t.len() && t[ti] != b'/' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                c if ti < t.len() && t[ti] == c => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        } else if ti == t.len() {
            return true;
        }

        // Mismatch — try to backtrack into a `*` (single segment) first.
        if let Some(spi) = star_pi
            && star_ti < t.len() && t[star_ti] != b'/' {
                star_ti += 1;
                pi = spi + 1;
                ti = star_ti;
                continue;
            }
            // The single `*` cannot consume a '/'; fall through to `**`, which
            // (if present) supersedes it. `star_pi` is not read again on this
            // path, so it need not be cleared here.
        // Fall back to the broader `**`, which may consume separators.
        if let Some(dpi) = dstar_pi
            && dstar_ti < t.len() {
                dstar_ti += 1;
                pi = dpi;
                ti = dstar_ti;
                star_pi = None;
                continue;
            }
        return false;
    }
    false
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(rules: &IgnoreRules, path: &str, is_dir: bool) -> bool {
        rules.is_ignored(path, is_dir)
    }

    // ── glob primitives ──────────────────────────────────────────────────────

    #[test]
    fn star_matches_within_segment_only() {
        assert!(glob_match("*.log", "error.log"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(!glob_match("*.log", "dir/error.log"), "single * must not cross /");
    }

    #[test]
    fn question_matches_one_non_slash() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "a/c"));
    }

    #[test]
    fn double_star_crosses_segments() {
        assert!(glob_match("**/foo", "foo"));
        assert!(glob_match("**/foo", "a/b/foo"));
        assert!(glob_match("a/**/z", "a/z"));
        assert!(glob_match("a/**/z", "a/b/c/z"));
        assert!(glob_match("src/**", "src/a/b.rs"));
    }

    #[test]
    fn literal_match() {
        assert!(glob_match("README.md", "README.md"));
        assert!(!glob_match("README.md", "README.markdown"));
    }

    // ── .tack always ignored ─────────────────────────────────────────────────

    #[test]
    fn tack_dir_always_ignored_even_with_empty_rules() {
        let rules = IgnoreRules::empty();
        assert!(ignored(&rules, ".tack", true));
        assert!(ignored(&rules, ".tack/objects/aa/bb", false));
    }

    #[test]
    fn tack_dir_cannot_be_unignored_by_negation() {
        // Even an explicit "!.tack" must not re-include the control dir.
        let rules = IgnoreRules::parse("!.tack\n");
        assert!(ignored(&rules, ".tack", true));
    }

    // ── basename matching (unanchored, slash-free) ───────────────────────────

    #[test]
    fn slash_free_pattern_matches_basename_at_any_depth() {
        let rules = IgnoreRules::parse("*.log\n");
        assert!(ignored(&rules, "error.log", false));
        assert!(ignored(&rules, "deep/nested/error.log", false));
        assert!(!ignored(&rules, "error.txt", false));
    }

    #[test]
    fn literal_name_matches_anywhere() {
        let rules = IgnoreRules::parse("target\n");
        assert!(ignored(&rules, "target", true));
        assert!(ignored(&rules, "crate/target", true));
        assert!(!ignored(&rules, "targets", true));
    }

    // ── anchoring ────────────────────────────────────────────────────────────

    #[test]
    fn leading_slash_anchors_to_root() {
        let rules = IgnoreRules::parse("/build\n");
        assert!(ignored(&rules, "build", true));
        assert!(!ignored(&rules, "sub/build", true), "anchored rule must not match nested");
    }

    #[test]
    fn interior_slash_anchors_pattern() {
        let rules = IgnoreRules::parse("src/generated\n");
        assert!(ignored(&rules, "src/generated", true));
        assert!(!ignored(&rules, "other/src/generated", true));
    }

    #[test]
    fn double_star_prefix_matches_at_any_depth() {
        let rules = IgnoreRules::parse("**/node_modules\n");
        assert!(ignored(&rules, "node_modules", true));
        assert!(ignored(&rules, "a/b/node_modules", true));
    }

    // ── directory-only ───────────────────────────────────────────────────────

    #[test]
    fn trailing_slash_matches_directories_only() {
        let rules = IgnoreRules::parse("cache/\n");
        assert!(ignored(&rules, "cache", true), "dir matches dir-only rule");
        assert!(!ignored(&rules, "cache", false), "file must not match dir-only rule");
        assert!(ignored(&rules, "a/cache", true));
    }

    // ── negation / last-match-wins ───────────────────────────────────────────

    #[test]
    fn negation_reincludes() {
        let rules = IgnoreRules::parse("*.log\n!keep.log\n");
        assert!(ignored(&rules, "debug.log", false));
        assert!(!ignored(&rules, "keep.log", false), "negation must re-include");
    }

    #[test]
    fn last_match_wins_order_matters() {
        // Re-ignore after a negation.
        let rules = IgnoreRules::parse("*.log\n!keep.log\nkeep.log\n");
        assert!(ignored(&rules, "keep.log", false));
    }

    // ── comments / blanks ────────────────────────────────────────────────────

    #[test]
    fn comments_and_blank_lines_skipped() {
        let rules = IgnoreRules::parse("# a comment\n\n   \n*.tmp\n");
        assert_eq!(rules.rules.len(), 1);
        assert!(ignored(&rules, "x.tmp", false));
    }

    #[test]
    fn escaped_hash_is_literal() {
        let rules = IgnoreRules::parse("\\#notacomment\n");
        assert!(ignored(&rules, "#notacomment", false));
    }

    // ── windows separators ───────────────────────────────────────────────────

    #[test]
    fn backslash_paths_are_normalized() {
        let rules = IgnoreRules::parse("src/generated\n");
        assert!(rules.is_ignored("src\\generated", true));
    }

    // ── load ─────────────────────────────────────────────────────────────────

    #[test]
    fn load_missing_file_is_empty() -> Result<()> {
        let rules = IgnoreRules::load("definitely-no-such-tackignore-file")?;
        assert!(!ignored(&rules, "anything", false));
        Ok(())
    }
}
