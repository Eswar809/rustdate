"""Generate benchmark.png — dateutil (Python) vs rustdate (Rust) bar chart.

By default plots the measured numbers from README (2026-09-06). Re-measure live
with:  python plot_bench.py --measure
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

import rustdate

# (label, dateutil µs, rustdate.parse µs, rustdate.parse_many µs-or-None)
# measured 2026-09-06, 20k parses/case, best of 7 — same numbers as README
MEASURED = [
    ("ISO 8601 + Z", 53.4, 4.31, None),
    ("ISO + numeric offset", 59.4, 4.57, None),
    ("Human (Sep 6 2026 2:23 PM)", 71.2, 4.06, None),
    ("US slash (06/09/2026 ...)", 45.9, 4.14, None),
    ("HTTP / RFC 7231", 88.9, 5.22, None),
    ("Time-only (14:23:45)", 21.3, 2.84, None),
    ("Fuzzy text", 95.7, 7.54, None),
    ("Mixed 10k workload", 66.1, 4.55, 0.66),
]

N = 10000


def measure():
    import time

    from dateutil import parser as dup

    CASES = [
        ("ISO 8601 + Z", "2026-09-06T14:23:45Z", {}),
        ("ISO + numeric offset", "2026-09-06 14:23:45+05:30", {}),
        ("Human (Sep 6 2026 2:23 PM)", "Sep 6 2026 2:23 PM", {}),
        ("US slash (06/09/2026 ...)", "06/09/2026 14:23:45", {}),
        ("HTTP / RFC 7231", "Sun, 06 Sep 2026 14:23:45 GMT", {}),
        ("Time-only (14:23:45)", "14:23:45", {}),
        ("Fuzzy text", "I met him on Sep 6 2026 at 2 PM", {"fuzzy": True}),
    ]

    def best_us(fn, n=5):
        best = float("inf")
        for _ in range(n):
            t0 = time.perf_counter()
            for _ in range(N):
                fn()
            best = min(best, time.perf_counter() - t0)
        return best / N * 1e6

    rows = []
    for name, s, kw in CASES:
        rows.append((name,
                     best_us(lambda: dup.parse(s, **kw)),
                     best_us(lambda: rustdate.parse(s, **kw)),
                     None))
    base = [c[1] for c in CASES[:5]]
    dates = base * 2000
    scale = len(dates) / N
    rows.append(("Mixed 10k workload",
                 best_us(lambda: [dup.parse(d) for d in dates], n=5) / scale,
                 best_us(lambda: [rustdate.parse(d) for d in dates], n=5) / scale,
                 best_us(lambda: rustdate.parse_many(dates), n=5) / scale))
    return rows


def plot(rows):
    labels = [r[0] for r in rows]
    du = [r[1] for r in rows]
    rd = [r[2] for r in rows]
    ba = [r[3] for r in rows]
    has_batch = ba[-1] is not None

    fig, ax = plt.subplots(figsize=(11.5, 6.5), dpi=150)
    y = np.arange(len(labels))
    h = 0.38

    ax.barh(y - h / 2, du, h, label="dateutil (Python)", color="#3776AB")
    ax.barh(y + h / 2, rd, h, label="rustdate.parse (Rust)", color="#DE6437")
    batch_y = y[-1] + h / 2 + h * 1.1
    if has_batch:
        ax.barh(batch_y, [ba[-1]], h,
                label="rustdate.parse_many (parallel batch)", color="#2E8B57")

    ax.set_yticks(y)
    ax.set_yticklabels(labels)
    ax.invert_yaxis()
    ax.set_xscale("log")
    ax.set_xlabel("µs per string (log scale) — lower is better")
    ax.set_title("dateutil (Python) vs rustdate (Rust) — date parsing speed",
                 fontsize=13, fontweight="bold", pad=34)
    ax.legend(loc="lower left", bbox_to_anchor=(0.0, 1.005), ncols=3,
              fontsize=9, frameon=False)

    for bars in ax.containers:
        for rect in bars:
            w = rect.get_width()
            if w > 0:
                ax.annotate(f"{w:.2f}",
                            xy=(w, rect.get_y() + rect.get_height() / 2),
                            xytext=(4, 0), textcoords="offset points",
                            va="center", fontsize=8, color="#333333")

    # speedup badges outside the axes, aligned with each group
    for i, r in enumerate(rows):
        best_rd = ba[i] if ba[i] is not None else rd[i]
        gy = batch_y if (has_batch and i == len(rows) - 1) else y[i]
        ax.annotate(f"{r[1] / best_rd:.0f}x",
                    xy=(1.015, gy), xycoords=("axes fraction", "data"),
                    ha="left", va="center", fontsize=11,
                    fontweight="bold", color="#B22222",
                    annotation_clip=False)

    ax.grid(axis="x", which="both", alpha=0.25)
    fig.text(0.99, 0.01,
             f"i5-12500H · CPython 3.12 · dateutil 2.9.0 vs rustdate {rustdate.__version__} · "
             f"best of 7 · 20k parses/case · 2026-09-06",
             ha="right", fontsize=7, color="#777777")
    fig.subplots_adjust(left=0.24, right=0.89, top=0.86, bottom=0.12)
    out = os.path.join(HERE, "benchmark.png")
    fig.savefig(out)
    print("saved", out)
    for r in rows:
        print(f"  {r[0]:32s} du={r[1]:7.2f}  rd={r[2]:6.2f}  batch={r[3]}")


if __name__ == "__main__":
    plot(measure() if "--measure" in sys.argv else MEASURED)
