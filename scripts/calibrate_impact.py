#!/usr/bin/env python3
"""Calibrate the `cship.impact` saturation constant K against real data.

The impact score is `score = 100 * raw / (raw + K)` where
    raw = commit_w*commits + merge_w*merges + eff_w*efficiency + breadth_w*files - thrash

K sets where real sessions land on the 0-100 curve. This script reconstructs
realistic *coding sessions* from git history (commits grouped by author with a
time-gap heuristic) and pairs them with a real per-session cost sampled from
Claude Code transcript token usage, then sweeps K and reports the resulting score
distribution so we can pick a K that spreads typical sessions across the range.

Usage:
    calibrate_impact.py REPO [REPO ...] [--transcripts DIR] [--months N] [--gap-hours H]

Everything here mirrors the defaults in `src/modules/impact.rs`. Re-run after
changing weights to re-pick K.
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import subprocess
import sys
from statistics import median

# ── Weights: keep in sync with src/modules/impact.rs defaults ────────────────
COMMIT_W = 4.0
MERGE_W = 8.0
EFF_W = 1.0
BREADTH_W = 1.0
CHURN_PER_DOLLAR_SCALE = 200.0

# Blended Claude pricing ($ per token) — approximate, only used to turn transcript
# token counts into a representative session cost for the (secondary) efficiency
# term. Order-of-magnitude is what matters; shipped/breadth dominate raw.
PRICE = {
    "input_tokens": 15.0 / 1e6,
    "output_tokens": 75.0 / 1e6,
    "cache_creation_input_tokens": 18.75 / 1e6,
    "cache_read_input_tokens": 1.50 / 1e6,
}


def git_sessions(repo: str, months: int, gap_hours: float):
    """Yield per-session dicts: {commits, merges, files, churn} for one repo.

    A session = consecutive commits by the same author email with < gap_hours
    between them (the same heuristic a human would use to define "a sitting").
    """
    gap = gap_hours * 3600
    # %x1f = unit separator; %P has parents (>=2 tokens => merge commit).
    fmt = "C\x1f%H\x1f%ae\x1f%ct\x1f%P"
    out = subprocess.run(
        ["git", "-C", repo, "log", f"--since={months} months ago",
         "--numstat", f"--pretty=format:{fmt}"],
        capture_output=True, text=True,
    ).stdout

    commits = []  # (author, ts, is_merge, [(churn, path)...])
    cur = None
    for line in out.splitlines():
        if line.startswith("C\x1f"):
            if cur:
                commits.append(cur)
            _, h, ae, ct, parents = line.split("\x1f")
            is_merge = len(parents.split()) >= 2
            cur = (ae, int(ct), is_merge, [])
        elif line.strip() and cur is not None:
            cols = line.split("\t")
            if len(cols) == 3:
                added = int(cols[0]) if cols[0].isdigit() else 0
                removed = int(cols[1]) if cols[1].isdigit() else 0
                cur[3].append((added + removed, cols[2]))
    if cur:
        commits.append(cur)

    # Group per author, sort by time, split on gap.
    by_author: dict[str, list] = {}
    for c in commits:
        by_author.setdefault(c[0], []).append(c)

    for author, cs in by_author.items():
        cs.sort(key=lambda c: c[1])
        session = []
        last_ts = None
        for c in cs:
            if last_ts is not None and c[1] - last_ts > gap:
                yield _summarize(session)
                session = []
            session.append(c)
            last_ts = c[1]
        if session:
            yield _summarize(session)


def _summarize(session):
    files = set()
    churn = 0
    merges = 0
    for _ae, _ts, is_merge, changes in session:
        merges += 1 if is_merge else 0
        for ch, path in changes:
            churn += ch
            files.add(path)
    return {
        "commits": len(session),
        "merges": merges,
        "files": len(files),
        "churn": churn,
    }


def transcript_costs(tdir: str):
    """Return a list of per-transcript $ costs from real Claude Code token usage."""
    costs = []
    for path in glob.glob(os.path.join(tdir, "**", "*.jsonl"), recursive=True):
        total = 0.0
        try:
            with open(path) as f:
                for line in f:
                    try:
                        obj = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    usage = (obj.get("message") or {}).get("usage") or obj.get("usage")
                    if isinstance(usage, dict):
                        for k, price in PRICE.items():
                            total += usage.get(k, 0) * price
        except OSError:
            continue
        if total > 0:
            costs.append(total)
    return costs


def raw_score(s, cost_usd):
    shipped = COMMIT_W * s["commits"] + MERGE_W * s["merges"]
    efficiency = (s["churn"] / cost_usd) / CHURN_PER_DOLLAR_SCALE if cost_usd > 0 else 0.0
    breadth = BREADTH_W * s["files"]
    return max(0.0, shipped + EFF_W * efficiency + breadth)


def pct(xs, p):
    if not xs:
        return 0
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("repos", nargs="+")
    ap.add_argument("--transcripts", default=os.path.expanduser("~/.claude/projects"))
    ap.add_argument("--months", type=int, default=12)
    ap.add_argument("--gap-hours", type=float, default=3.0)
    args = ap.parse_args()

    sessions = []
    for repo in args.repos:
        sessions.extend(git_sessions(repo, args.months, args.gap_hours))
    if not sessions:
        sys.exit("no sessions found")

    costs = transcript_costs(args.transcripts)
    cost_rep = median(costs) if costs else 2.0

    print(f"sessions: {len(sessions)} across {len(args.repos)} repo(s), "
          f"gap={args.gap_hours}h, window={args.months}mo")
    print(f"transcript costs: n={len(costs)} "
          f"median=${cost_rep:.2f} p25=${pct(costs,25):.2f} p75=${pct(costs,75):.2f}")
    print("\nsession activity percentiles:")
    for key in ("commits", "merges", "files", "churn"):
        vals = [s[key] for s in sessions]
        print(f"  {key:8} p10={pct(vals,10):>5} p50={pct(vals,50):>5} "
              f"p90={pct(vals,90):>5} max={max(vals):>6}")

    raws = [raw_score(s, cost_rep) for s in sessions]
    print(f"\nraw score: p10={pct(raws,10):.1f} p50={pct(raws,50):.1f} "
          f"p90={pct(raws,90):.1f}")

    print("\nK sweep — resulting score (0-100) percentiles:")
    print(f"  {'K':>4} | {'p10':>5} {'p25':>5} {'p50':>5} {'p75':>5} {'p90':>5}")
    for k in (5, 8, 10, 12, 15, 20, 25):
        scores = [round(100 * r / (r + k)) for r in raws]
        print(f"  {k:>4} | {pct(scores,10):>5} {pct(scores,25):>5} "
              f"{pct(scores,50):>5} {pct(scores,75):>5} {pct(scores,90):>5}")

    print("\nGoal: median (p50) landing ~55-65 with p10<30 and p90>85 for good spread.")


if __name__ == "__main__":
    main()
