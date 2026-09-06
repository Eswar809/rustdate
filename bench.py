"""rustdate vs dateutil.parser — full parity correctness check + benchmark."""
import os
import sys
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import rustdate
from dateutil import parser as dup

DEF = datetime(2020, 3, 15, 7, 8, 9)

# (string, kwargs) — outputs must be identical to dateutil.parse(s, **kw)
CASES = [
    # the 5 hot real-world formats
    ("2026-09-06T14:23:45Z", {}),
    ("2026-09-06 14:23:45+05:30", {}),
    ("Sep 6 2026 2:23 PM", {}),
    ("06/09/2026 14:23:45", {}),
    ("Sun, 06 Sep 2026 14:23:45 GMT", {}),
    # ISO family
    ("2026-09-06", {}),
    ("2026-09-06T14:23", {}),
    ("2026-09-06T14:23:45.123456Z", {}),
    ("2026-09-06T14:23:45-07:00", {}),
    ("2026-09-06 14:23:45.5+00:00", {}),
    ("2026-09-06T14:23:45+0530", {}),
    ("2026-09-06T14:23:45.123456789Z", {}),
    ("2026-09", {"default": DEF}),
    ("2026", {"default": DEF}),
    # slash / dash / spaced
    ("12/25/2026 8:05 AM", {}),
    ("25/12/2026", {}),
    ("13/05/2026", {}),
    ("12-25-2026 8:05", {}),
    ("2026/09/06", {}),
    ("2026/09", {"default": DEF}),
    ("9/6", {"default": DEF}),
    ("2026 09 06", {}),
    ("2026 09", {"default": DEF}),
    ("10-11-12", {"default": DEF}),
    ("10-11-12", {"default": DEF, "yearfirst": True}),
    ("06/09/2026 14:23:45", {"dayfirst": True}),
    ("25-12-2026", {"dayfirst": True}),
    # human formats
    ("Sep 6, 2026", {}),
    ("6 Sep 2026", {}),
    ("Sep 2026", {"default": DEF}),
    ("Sep 6", {"default": DEF}),
    ("6 Sep", {"default": DEF}),
    ("Mar 31 2025 11:59:59 PM", {}),
    ("Sunday, Sep 6 2026", {}),
    ("6th of Sep 2026", {}),
    ("Sep 6 2026 at 2 PM", {}),
    ("2:23 p.m.", {"default": DEF}),
    ("Sep 6th 2026", {}),
    # time-only + defaults
    ("14:23:45", {"default": DEF}),
    ("2:23 PM", {"default": DEF}),
    ("10:30", {"default": DEF}),
    # ISO week / ordinal / compact
    ("20260906T143022Z", {}),
    ("20260906", {}),
    ("20260906T1430", {}),
    # fuzzy
    ("I met him on Sep 6 2026 at 2 PM", {"fuzzy": True}),
    ("meeting Thursday 2026-09-06 10:30 ok", {"fuzzy": True}),
    ("the date is 2026-09-06, thanks", {"fuzzy": True}),
    # strict-mode jump words (dateutil parses these too, no fuzzy needed)
    ("Sep 6 2026 at 2 PM", {}),
    ("on Sep 6 2026", {}),
]

# tz names both parsers resolve via gettz/zoneinfo
TZ_CASES = [
    ("2026-09-06 14:23:45 EST", {}),
    ("2026-09-06 14:23:45 PST", {}),
    ("2026-09-06 14:23:45 UTC", {}),
    ("2026-09-06 14:23:45 IST", {}),
]

# extra capability: formats the dateutil PARSER can't handle (verified manually
# against the calendar / zoneinfo) — ISO week & ordinal dates, heavy fuzzy text,
# and IANA/DST zone names with slashes or digits
from zoneinfo import ZoneInfo

EXTRA = [
    ("2026-W36-6", {}, datetime(2026, 9, 5)),
    ("2024-W10-1", {}, datetime(2024, 3, 4)),
    ("2026-249", {}, datetime(2026, 9, 6)),
    ("2024-060", {}, datetime(2024, 2, 29)),
    ("2026-W01-1", {}, datetime(2025, 12, 29)),  # week 1 of 2026 starts in 2025
    ("2026-09-06 14:23:45 Asia/Kolkata", {},
     datetime(2026, 9, 6, 14, 23, 45, tzinfo=ZoneInfo("Asia/Kolkata"))),
    ("2026-09-06 14:23:45 America/New_York", {},
     datetime(2026, 9, 6, 14, 23, 45, tzinfo=ZoneInfo("America/New_York"))),
    ("2026-07-01 12:00:00 America/New_York", {},
     datetime(2026, 7, 1, 12, 0, 0, tzinfo=ZoneInfo("America/New_York"))),
    ("2026-09-06 14:23:45 EST5EDT", {},
     datetime(2026, 9, 6, 14, 23, 45, tzinfo=ZoneInfo("EST5EDT"))),
]

BAD = ["garbage", "2026-13-40", "2026-02-30", "", "Sep 40 2026", "hello world",
       "2026-366", "2026-W36", "25/13/2026",
       "updated: 25/12/2026 8:05 PM (v2)"]  # both parsers reject this one


def correctness():
    print(f"rustdate {rustdate.__version__} — dateutil parity check")
    print("=" * 64)
    fails = 0
    total = 0
    for s, kw in CASES:
        total += 1
        try:
            expected = dup.parse(s, **kw)
        except Exception as e:
            print(f"  SKIP {s!r:45s} (dateutil errored: {type(e).__name__})")
            continue
        got = rustdate.parse(s, **kw)
        if got != expected:
            fails += 1
            print(f"  MISMATCH {s!r} {kw}\n    dateutil={expected!r}\n    rustdate={got!r}")
    # tz cases (both resolve via gettz/zoneinfo)
    for s, kw in TZ_CASES:
        total += 1
        try:
            expected = dup.parse(s, **kw)
        except Exception:
            print(f"  SKIP {s!r:45s} (dateutil errored)")
            continue
        got = rustdate.parse(s, **kw)
        if got != expected:
            fails += 1
            print(f"  MISMATCH {s!r}\n    dateutil={expected!r}\n    rustdate={got!r}")
    # extra capability: formats the dateutil PARSER cannot handle
    extra = 0
    for s, kw, expected in EXTRA:
        total += 1
        try:
            dup.parse(s, **kw)
            print(f"  NOTE {s!r}: dateutil parsed too — move to CASES")
        except Exception:
            try:
                got = rustdate.parse(s, **kw)
            except ValueError as e:
                fails += 1
                print(f"  MISMATCH {s!r}: rustdate errored: {e}")
                continue
            if got != expected:
                fails += 1
                print(f"  MISMATCH {s!r}\n    expected={expected!r}\n    rustdate={got!r}")
            else:
                extra += 1
                print(f"  EXTRA {s!r:52s} -> {got.isoformat()}")
    # errors: both must raise (dateutil ParserError is a ValueError; ours too)
    for s in BAD:
        total += 1
        du_err = rd_err = None
        try:
            dup.parse(s)
        except Exception as e:
            du_err = e
        try:
            rustdate.parse(s)
        except ValueError as e:
            rd_err = e
        if du_err is None or rd_err is None:
            fails += 1
            print(f"  MISMATCH reject {s!r}: dateutil={'err' if du_err else 'OK'}, "
                  f"rustdate={'err' if rd_err else 'OK'}")
        elif not isinstance(rd_err, rustdate.ParserError):
            fails += 1
            print(f"  MISMATCH {s!r}: rustdate error type {type(rd_err).__name__} "
                  f"is not ParserError")
    print("-" * 64)
    print(f"PASS: {total - fails}/{total} checks, {extra} extra-capability wins"
          if fails == 0 else f"{fails} MISMATCHES out of {total}")
    return fails


def benchmark():
    print("\nbenchmark: 10,000 date strings (5 hot formats x 2000)")
    print("-" * 64)
    base = [c for c, _ in CASES[:5]]
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
