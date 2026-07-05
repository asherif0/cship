//! Render the `[cship.impact]` module — a deterministic 0–100 session impact score.
//!
//! The score is a bounded, monotonic, legible combination of four signals:
//!   • **shipped work**  — commits + merges landed this session (local git; 0 without git)
//!   • **token efficiency** — code churn per dollar (`total_lines_added/removed` from the
//!                            statusline JSON — always available, no git needed)
//!   • **breadth**       — files touched (local git; 0 without git)
//!   • **anti-thrash**   — a penalty when cost is burned with nothing to show
//!
//! Numbers are never guessed: git counters are diffed against a per-session
//! baseline cached in [`crate::cache`], and the token signals come straight from
//! the context. Git runs at most once per `cache_ttl_secs`; the token terms are
//! recomputed live every render, so the score reacts each turn while staying cheap.
//!
//! Formula: `raw = w_c·commits + w_m·merges + w_e·(churn/$ ÷ scale) + w_b·files − thrash`,
//! then `score = round(100 · raw / (raw + K))` — saturating, so it stays in 0–100.
//! `K` (`saturation_k`) is the main calibration knob.

use std::path::Path;

use crate::config::{CshipConfig, ImpactConfig};
use crate::context::Context;
use crate::{cache, git};

const EPS: f64 = 1e-9;

// ── Defaults (documented in ImpactConfig) ────────────────────────────────────
const DEF_COMMIT_WEIGHT: f64 = 4.0;
const DEF_MERGE_WEIGHT: f64 = 8.0;
const DEF_EFFICIENCY_WEIGHT: f64 = 1.0;
const DEF_BREADTH_WEIGHT: f64 = 1.0;
const DEF_THRASH_PENALTY: f64 = 3.0;
const DEF_CHURN_PER_DOLLAR_SCALE: f64 = 200.0;
// Calibrated against 2404 real git sessions (see scripts/calibrate_impact.py):
// K=10 puts the median session at score ~60 and the top decile at ~86.
const DEF_SATURATION_K: f64 = 10.0;
const DEF_THRASH_COST_THRESHOLD: f64 = 0.10;
const DEF_CACHE_TTL_SECS: u64 = 5;
const DEF_WARN_THRESHOLD: f64 = 50.0;
const DEF_CRITICAL_THRESHOLD: f64 = 20.0;

/// Renders `$cship.impact` — the deterministic session impact score.
pub fn render(ctx: &Context, cfg: &CshipConfig) -> Option<String> {
    let icfg = cfg.impact.as_ref();

    // Disabled flag → silent None (project convention: no warn on disabled).
    if icfg.and_then(|c| c.disabled).unwrap_or(false) {
        return None;
    }

    // Working directory — prefer the explicit workspace dir, fall back to cwd.
    let cwd = ctx
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())
        .or(ctx.cwd.as_deref());
    let cwd = match cwd {
        Some(c) => c,
        None => {
            tracing::warn!("cship.impact: no cwd/workspace in context — cannot compute score");
            return None;
        }
    };

    let session_id = ctx.session_id.as_deref().unwrap_or("");
    let cost_usd = ctx
        .cost
        .as_ref()
        .and_then(|c| c.total_cost_usd)
        .unwrap_or(0.0);
    // Claude's own edit volume this session (token-side churn; git-independent).
    let code_lines = ctx
        .cost
        .as_ref()
        .map(|c| {
            let added = c.total_lines_added.unwrap_or(0).max(0) as u64;
            let removed = c.total_lines_removed.unwrap_or(0).max(0) as u64;
            added + removed
        })
        .unwrap_or(0);

    let ttl = icfg
        .and_then(|c| c.cache_ttl_secs)
        .unwrap_or(DEF_CACHE_TTL_SECS);
    let transcript = ctx.transcript_path.as_deref().map(Path::new);
    let cached = transcript.and_then(cache::read_impact);

    // Resolve the per-session git baseline + a current raw snapshot. Reuse the
    // cached snapshot while fresh; otherwise recompute via git (preserving the
    // baseline). A new/absent session establishes the baseline as "now".
    //
    // `keep_expires_at` carries the stored git TTL forward on a fresh render so
    // persisting the score every render (below) does not push the git expiry out
    // and starve the once-per-TTL git reads. It is `None` whenever we recomputed
    // the snapshot (then the write refreshes the TTL).
    let (base_commits, base_merges, raw, prev_score, keep_expires_at) = match cached {
        Some(c) if c.session_id == session_id && c.fresh => (
            c.baseline_commit_count,
            c.baseline_merge_count,
            c.raw,
            c.last_score,
            Some(c.expires_at),
        ),
        Some(c) if c.session_id == session_id => {
            let raw = git::read_raw(cwd);
            (
                c.baseline_commit_count,
                c.baseline_merge_count,
                raw,
                c.last_score,
                None,
            )
        }
        _ => {
            let raw = git::read_raw(cwd);
            (raw.commit_count, raw.merge_count, raw, 0, None)
        }
    };

    let commits = raw.commit_count.saturating_sub(base_commits);
    let merges = raw.merge_count.saturating_sub(base_merges);
    let files = raw.files;

    let score = compute_score(icfg, cost_usd, commits, merges, code_lines, files);

    // Persist every render so the delta arrow compares against the *previous
    // render's* score (its documented intent), not the last git recompute — the
    // token terms move the score between git reads, so a recompute-gated baseline
    // let the number change with no arrow. Preserve the git TTL on fresh renders
    // (`keep_expires_at`); refresh it only when the snapshot was recomputed.
    if let Some(tp) = transcript {
        cache::write_impact(
            tp,
            session_id,
            base_commits,
            base_merges,
            &raw,
            score,
            ttl,
            keep_expires_at,
        );
    }

    Some(format_output(icfg, score, prev_score))
}

/// Compute the bounded 0–100 score. Pure function of the inputs + config → fully
/// deterministic and unit-testable.
fn compute_score(
    icfg: Option<&ImpactConfig>,
    cost_usd: f64,
    commits: u64,
    merges: u64,
    code_lines: u64,
    files: u64,
) -> u32 {
    let commit_w = icfg
        .and_then(|c| c.commit_weight)
        .unwrap_or(DEF_COMMIT_WEIGHT);
    let merge_w = icfg
        .and_then(|c| c.merge_weight)
        .unwrap_or(DEF_MERGE_WEIGHT);
    let eff_w = icfg
        .and_then(|c| c.efficiency_weight)
        .unwrap_or(DEF_EFFICIENCY_WEIGHT);
    let breadth_w = icfg
        .and_then(|c| c.breadth_weight)
        .unwrap_or(DEF_BREADTH_WEIGHT);
    let thrash_p = icfg
        .and_then(|c| c.thrash_penalty)
        .unwrap_or(DEF_THRASH_PENALTY);
    let churn_scale = icfg
        .and_then(|c| c.churn_per_dollar_scale)
        .unwrap_or(DEF_CHURN_PER_DOLLAR_SCALE)
        .max(EPS);
    let k = icfg
        .and_then(|c| c.saturation_k)
        .unwrap_or(DEF_SATURATION_K)
        .max(EPS);
    let thrash_cost = icfg
        .and_then(|c| c.thrash_cost_threshold)
        .unwrap_or(DEF_THRASH_COST_THRESHOLD);

    let shipped = commit_w * commits as f64 + merge_w * merges as f64;

    // Token efficiency: code churn per dollar, scaled to O(1). Zero when cost is
    // negligible (avoids div-by-zero and rewards for "free" work at session start).
    let efficiency = if cost_usd > EPS {
        (code_lines as f64 / cost_usd) / churn_scale
    } else {
        0.0
    };

    let breadth = breadth_w * files as f64;

    // Anti-thrash: cost burned with zero shipped work and zero edits.
    let no_output = commits == 0 && merges == 0 && code_lines == 0;
    let thrash = if no_output && cost_usd > thrash_cost {
        thrash_p
    } else {
        0.0
    };

    let raw_score = (shipped + eff_w * efficiency + breadth - thrash).max(0.0);
    let score = 100.0 * raw_score / (raw_score + k);
    score.round().clamp(0.0, 100.0) as u32
}

/// Build the styled output string: `{symbol}{label}{score}{ delta}`, painted with
/// floor-threshold escalation (lower score = more severe), and routed through a
/// custom `format` string when configured (mirrors the cost module).
fn format_output(icfg: Option<&ImpactConfig>, score: u32, prev_score: u32) -> String {
    let style = icfg.and_then(|c| c.style.as_deref());
    let warn_t = icfg
        .and_then(|c| c.warn_threshold)
        .unwrap_or(DEF_WARN_THRESHOLD);
    let warn_s = icfg.and_then(|c| c.warn_style.as_deref());
    let crit_t = icfg
        .and_then(|c| c.critical_threshold)
        .unwrap_or(DEF_CRITICAL_THRESHOLD);
    let crit_s = icfg.and_then(|c| c.critical_style.as_deref());
    let effective_style = band_style(score as f64, style, warn_t, warn_s, crit_t, crit_s);

    let symbol = icfg.and_then(|c| c.symbol.as_deref());
    let show_delta = icfg.and_then(|c| c.show_delta).unwrap_or(true);
    let delta = if show_delta {
        delta_marker(score, prev_score)
    } else {
        String::new()
    };
    let value = format!("{score}{delta}");

    // Custom format string takes priority (AC parity with cost::render).
    if let Some(fmt) = icfg.and_then(|c| c.format.as_deref())
        && let Some(out) =
            crate::format::apply_module_format(fmt, Some(&value), symbol, effective_style)
    {
        return out;
    }

    let label = if icfg.and_then(|c| c.label).unwrap_or(false) {
        "impact "
    } else {
        ""
    };
    let symbol_str = symbol.unwrap_or("");
    let content = format!("{symbol_str}{label}{value}");
    crate::ansi::apply_style(&content, effective_style)
}

/// A trailing delta marker (` ▲+N` / ` ▼-N`) vs the previous rendered score, or
/// empty when unchanged.
fn delta_marker(score: u32, prev: u32) -> String {
    match score.cmp(&prev) {
        std::cmp::Ordering::Greater => format!(" ▲+{}", score - prev),
        std::cmp::Ordering::Less => format!(" ▼-{}", prev - score),
        std::cmp::Ordering::Equal => String::new(),
    }
}

/// Floor-threshold style selection: escalates as the score *drops* (opposite of
/// cost's ceiling thresholds). Only escalates when the matching style is set.
fn band_style<'a>(
    score: f64,
    style: Option<&'a str>,
    warn_t: f64,
    warn_s: Option<&'a str>,
    crit_t: f64,
    crit_s: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(s) = crit_s
        && score <= crit_t
    {
        return Some(s);
    }
    if let Some(s) = warn_s
        && score <= warn_t
    {
        return Some(s);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_is_zero_at_session_start_no_activity() {
        // No commits, no lines, no cost → raw 0 → score 0.
        assert_eq!(compute_score(None, 0.0, 0, 0, 0, 0), 0);
    }

    #[test]
    fn shipping_commits_raises_score() {
        let none = compute_score(None, 1.0, 0, 0, 0, 0);
        let one = compute_score(None, 1.0, 1, 0, 0, 0);
        let two = compute_score(None, 1.0, 2, 0, 0, 0);
        assert!(one > none);
        assert!(two > one, "score is monotonic in commits");
    }

    #[test]
    fn merges_outweigh_plain_commits() {
        let commit = compute_score(None, 1.0, 1, 0, 0, 0);
        let merge = compute_score(None, 1.0, 0, 1, 0, 0);
        assert!(merge > commit, "a merge is weighted above a bare commit");
    }

    #[test]
    fn score_stays_bounded_0_100() {
        let huge = compute_score(None, 1.0, 10_000, 10_000, 1_000_000, 10_000);
        assert!(huge <= 100);
    }

    #[test]
    fn thrash_penalty_only_when_cost_and_no_output() {
        // Cost burned, nothing produced → stays 0 (penalty clamps raw at 0).
        assert_eq!(compute_score(None, 5.0, 0, 0, 0, 0), 0);
        // Below the cost threshold → no penalty applied (still 0 here, but the
        // penalty branch is not taken).
        assert_eq!(compute_score(None, 0.01, 0, 0, 0, 0), 0);
    }

    #[test]
    fn efficiency_rewards_output_per_dollar() {
        let cheap = compute_score(None, 0.5, 0, 0, 400, 0);
        let pricey = compute_score(None, 5.0, 0, 0, 400, 0);
        assert!(cheap > pricey, "same output for less money scores higher");
    }

    #[test]
    fn delta_marker_formats_direction() {
        assert_eq!(delta_marker(60, 56), " ▲+4");
        assert_eq!(delta_marker(50, 55), " ▼-5");
        assert_eq!(delta_marker(50, 50), "");
    }

    #[test]
    fn band_style_uses_floor_semantics() {
        // low score → critical; mid → warn; high → base
        assert_eq!(
            band_style(10.0, Some("base"), 50.0, Some("warn"), 20.0, Some("crit")),
            Some("crit")
        );
        assert_eq!(
            band_style(40.0, Some("base"), 50.0, Some("warn"), 20.0, Some("crit")),
            Some("warn")
        );
        assert_eq!(
            band_style(80.0, Some("base"), 50.0, Some("warn"), 20.0, Some("crit")),
            Some("base")
        );
    }
}
