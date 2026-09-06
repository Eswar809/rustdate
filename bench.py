"""rustdate vs dateutil.parser — correctness check + benchmark."""
import os
import sys
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import rustdate
from dateutil import parser as dup

SAMPLES = [
    # the 5 hot real-world formats
    "2026-09-06T14:23:45Z",
    "2026-09-06 14:23:45+05:30",
    "Sep 6 2026 2:23 PM",
    "06/09/2026 14:23:45",
    "Sun, 06 Sep 2026 14:23:45 GMT",
    # extra coverage
    "2026-09-06",
    "2026-09-06T14:23",
    "Sep 6, 2026",
    "6 Sep 2026",
    "2026-09-06T14:23:45.123456Z",
    "2026-09-06T14:23:45-07:00",
    "2026-09-06 14:23:45.5+00:00",
    "12/25/2026 8:05 AM",
    "2026-09-06T14:23:45+0530",
    "Mar 31 2025 11:59:59 PM",
]

BAD = ["garbage", "2026-13-40", "2026-02-30", "", "Sep 40 2026", "hello world"]


def correctness():
    print(f"rustdate {rustdate.__version__} — correctness vs dateutil")
    print("-" * 60)
    fails = 0
    for s in SAMPLES:
        try:
            expected = dup.parse(s)
        except Exception as e:  # dateutil itself can't do it
            print(f"  SKIP {s!r:40s} (dateutil errored: {type(e).__name__})")
            continue
        got = rustdate.parse(s)
        if got != expected or not isinstance(got, datetime):
            fails += 1
            print(f"  MISMATCH {s!r}\n    dateutil={expected!r}\n    rustdate={got!r}")
        else:
            print(f"  OK    {s!r:40s} -> {got.isoformat()}")
    # dayfirst / dashed-date coverage
    for s, kw in [
        ("06/09/2026 14:23:45", {"dayfirst": True}),
        ("25/12/2026", {}),
        ("25/12/2026", {"dayfirst": True}),
        ("13/05/2026", {}),
        ("12-25-2026 8:05", {}),
        ("25-12-2026", {"dayfirst": True}),
    ]:
        expected = dup.parse(s, **kw)
        got = rustdate.parse(s, **kw)
        if got != expected:
            fails += 1
            print(f"  MISMATCH {s!r} {kw}\n    dateutil={expected!r}\n    rustdate={got!r}")
        else:
            print(f"  OK    {s!r:40s} {kw} -> {got.isoformat()}")
    for s in BAD:
        du_err = rd_err = False
        try:
            dup.parse(s)
        except Exception:
            du_err = True
        try:
            rustdate.parse(s)
        except ValueError:
            rd_err = True
        status = "OK   " if (du_err and rd_err) else "DIFF "
        if not (du_err and rd_err):
            fails += 1
        print(f"  {status} reject {s!r:40s} (dateutil errs={du_err}, rustdate errs={rd_err})")
    print("-" * 60)
    print("PASS: outputs identical to dateutil" if fails == 0 else f"{fails} MISMATCHES")
    return fails


def benchmark():
    print("\nbenchmark: 10,000 date strings (5 hot formats x 2000)")
    print("-" * 60)
    base = SAMPLES[:5]
    dates = base * 2000

    def best_of(fn, n=7):
        best = float("inf")
        for _ in range(n):
            t0 = time.perf_counter()
            for d in dates:
                fn(d)
            best = min(best, time.perf_counter() - t0)
        return best

    t_du = best_of(dup.parse)
    t_rd = best_of(rustdate.parse)

    t_batch = float("inf")
    for _ in range(7):
        t0 = time.perf_counter()
        rustdate.parse_many(dates)
        t_batch = min(t_batch, time.perf_counter() - t0)

    print(f"  dateutil.parse      {t_du * 1e6 / len(dates):8.2f} us/string  ({t_du * 1e3:8.1f} ms total)")
    print(f"  rustdate.parse      {t_rd * 1e6 / len(dates):8.2f} us/string  ({t_rd * 1e3:8.1f} ms total)")
    print(f"  rustdate.parse_many {t_batch * 1e6 / len(dates):8.2f} us/string  ({t_batch * 1e3:8.1f} ms total)  [rayon parallel]")
    print(f"\n  speedup (single-call): {t_du / t_rd:,.0f}x")
    print(f"  speedup (batch):       {t_du / t_batch:,.0f}x")


if __name__ == "__main__":
    fails = correctness()
    if fails:
        sys.exit(1)
    benchmark()
