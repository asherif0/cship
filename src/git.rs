//! Local git signal collection for the `cship.impact` module.
//!
//! Reads the working repository via `git` subprocess calls — **local only, no
//! network, no auth** (deliberate design choice for the impact score: offline
//! and fast). Captures raw counters that the `impact` module diffs against a
//! per-session baseline to derive "shipped this session" numbers.
//!
//! All calls are cheap (`git rev-list --count`, `git diff --numstat`) and are
//! invoked at most once per cache TTL (see [`crate::cache::read_impact`]), never
//! on every statusline render. Any failure — not a repo, git not installed,
//! detached/empty repo — degrades to `available: false` with zeroed counters;
//! the impact score simply falls back to the token-only signals.

use std::process::Command;

/// Raw git counters at a single point in time. The `impact` module subtracts a
/// stored session baseline from `commit_count` / `merge_count` to get the
/// per-session deltas; `churn` and `files` are inherently "current working tree".
///
/// Serializable so the snapshot can be cached between renders (Story: impact cache).
/// Forward-compatible — no `deny_unknown_fields` (matches the project convention).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GitRaw {
    /// Total commits reachable from HEAD (`git rev-list --count HEAD`).
    pub commit_count: u64,
    /// Total merge commits reachable from HEAD (`git rev-list --count --merges HEAD`).
    pub merge_count: u64,
    /// Uncommitted churn vs HEAD: sum of added + removed lines (`git diff HEAD --numstat`).
    pub churn: u64,
    /// Number of files with uncommitted changes vs HEAD.
    pub files: u64,
    /// `false` when `cwd` is not a git repo / git is unavailable — score ignores git terms.
    pub available: bool,
}

/// Run `git -C <cwd> <args...>` and return trimmed stdout, or `None` on any
/// failure (non-zero exit, spawn error, non-UTF8). Never panics.
fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Collect the current raw git counters for `cwd`.
///
/// Returns `GitRaw { available: false, .. }` (all zeros) when `cwd` is not a git
/// repository or has no commits yet — the caller treats git terms as neutral.
pub fn read_raw(cwd: &str) -> GitRaw {
    // Cheap repo probe first — avoids three failing subprocesses in a non-repo.
    if git(cwd, &["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        return GitRaw::default();
    }

    // Empty repo (no HEAD yet): rev-list fails. Treat as available-but-zero.
    let commit_count = git(cwd, &["rev-list", "--count", "HEAD"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let merge_count = git(cwd, &["rev-list", "--count", "--merges", "HEAD"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let (churn, files) = git(cwd, &["diff", "HEAD", "--numstat"])
        .map(|out| parse_numstat(&out))
        .unwrap_or((0, 0));

    GitRaw {
        commit_count,
        merge_count,
        churn,
        files,
        available: true,
    }
}

/// Parse `git diff --numstat` output into `(total_churn, file_count)`.
///
/// Each line is `<added>\t<removed>\t<path>`. Binary files report `-` for the
/// counts — they contribute to the file count but add 0 churn.
fn parse_numstat(out: &str) -> (u64, u64) {
    let mut churn = 0u64;
    let mut files = 0u64;
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        files += 1;
        let mut cols = line.split('\t');
        let added: u64 = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let removed: u64 = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        churn += added + removed;
    }
    (churn, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numstat_sums_added_and_removed() {
        let out = "10\t2\tsrc/a.rs\n0\t5\tsrc/b.rs";
        assert_eq!(parse_numstat(out), (17, 2));
    }

    #[test]
    fn parse_numstat_handles_binary_and_blank_lines() {
        let out = "-\t-\tlogo.png\n\n3\t1\tsrc/c.rs\n";
        // binary file: 0 churn, still counted; blank line ignored
        assert_eq!(parse_numstat(out), (4, 2));
    }

    #[test]
    fn parse_numstat_empty_is_zero() {
        assert_eq!(parse_numstat(""), (0, 0));
    }

    #[test]
    fn read_raw_on_non_repo_is_unavailable() {
        // A temp dir with no git repo → unavailable, all zero.
        let dir = tempfile::tempdir().expect("tempdir");
        let raw = read_raw(dir.path().to_str().unwrap());
        assert!(!raw.available);
        assert_eq!(raw.commit_count, 0);
    }
}
