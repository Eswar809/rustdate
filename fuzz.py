"""Differential fuzzer: rustdate vs dateutil.parser on random generated inputs.

For every generated case both parsers run with identical kwargs and default.
- both succeed  -> datetimes must be equal
- both error    -> OK
- dateutil errors, rustdate succeeds -> counted as "extra" (accepted: features
  dateutil's parser lacks — ISO week/ordinal, fuzzy leftovers, IANA zones)
- rustdate errors, dateutil succeeds -> FAILURE
- both succeed, different output -> FAILURE
"""
import os
import random
import sys
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import rustdate
from dateutil import parser as dup

DEF = datetime(2020, 3, 15, 7, 8, 9)
rng = random.Random(20260906)

YEARS4 = [2026, 2024, 2023, 1999, 2001, 2030, 2015]
YEARS2 = ["26", "24", "99", "01", "15"]
MONTHS = list(range(1, 13))
DAYS = list(range(1, 29)) + [29, 30, 31]
HOURS = list(range(0, 24))
MINSEC = list(range(0, 60))
MONTH_NAMES = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
               "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
WEEKDAYS = ["Mon", "Tuesday", "Wed", "Thu", "Friday", "Sat", "Sun"]
OFFSETS = ["Z", "+05:30", "-07:00", "+00:00", "+0530", "-03", "GMT", "UTC"]


def g_iso():
    y = rng.choice(YEARS4)
    m = rng.choice(MONTHS)
    d = rng.choice(DAYS)
    s = f"{y:04d}-{m:02d}-{d:02d}"
    if rng.random() < 0.75:
        h = rng.choice(HOURS)
        mi = rng.choice(MINSEC)
        s += rng.choice(["T", " ", "t"]) + f"{h:02d}:{mi:02d}"
        if rng.random() < 0.7:
            sec = rng.choice(MINSEC)
            s += f":{sec:02d}"
            if rng.random() < 0.3:
                s += "." + rng.choice(["5", "25", "123456", "123456789"])
        if rng.random() < 0.4:
            s += rng.choice(OFFSETS)
    return s, {}


def g_slash_dash():
    sep = rng.choice(["/", "-"])
    parts = 3 if rng.random() < 0.8 else 2
    m = rng.choice(MONTHS)
    d = rng.choice(DAYS)
    y = rng.choice(YEARS4) if rng.random() < 0.8 else rng.choice(YEARS2)
    if parts == 3:
        order = rng.choice(["mdy", "dmy", "ymd"])
        comps = {"mdy": [m, d, y], "dmy": [d, m, y], "ymd": [y, m, d]}[order]
    else:
        comps = [m, d] if rng.random() < 0.7 else [y, m]
    s = sep.join(str(c).zfill(2) for c in comps)
    if rng.random() < 0.4:
        s += f" {rng.choice(HOURS):02d}:{rng.choice(MINSEC):02d}"
        if rng.random() < 0.25:
            s += " PM" if rng.random() < 0.5 else " AM"
    return s, {"dayfirst": rng.random() < 0.3, "yearfirst": rng.random() < 0.2}


def g_human():
    mo = rng.choice(MONTH_NAMES)
    d = rng.choice(DAYS[:28] + [29, 30])
    y = rng.choice(YEARS4 + YEARS2)
    style = rng.choice(["mdy", "mdy_comma", "dmy", "mon_y", "mon_d",
                        "weekday_mdy", "dmy_suffix"])
    if style == "mdy":
        s = f"{mo} {d} {y}"
    elif style == "mdy_comma":
        s = f"{mo} {d}, {y}"
    elif style == "dmy":
        s = f"{d} {mo} {y}"
    elif style == "mon_y":
        s = f"{mo} {y}"
    elif style == "mon_d":
        s = f"{mo} {d}"
    elif style == "weekday_mdy":
        s = f"{rng.choice(WEEKDAYS)}, {mo} {d} {y}"
    else:
        s = f"{d}th {mo} {y}"
    if rng.random() < 0.5:
        h = rng.choice(list(range(1, 13)))
        mi = rng.choice(MINSEC)
        ap = rng.choice(["AM", "PM", "am", "pm", "p.m.", "a.m."])
        s += f" at {h}:{mi:02d} {ap}" if rng.random() < 0.5 else f" {h}:{mi:02d} {ap}"
    return s, {}


def g_compact():
    y = rng.choice(YEARS4)
    m = rng.choice(MONTHS)
    d = rng.choice(DAYS)
    s = f"{y:04d}{m:02d}{d:02d}"
    if rng.random() < 0.5:
        s += "T" + f"{rng.choice(HOURS):02d}{rng.choice(MINSEC):02d}"
        if rng.random() < 0.5:
            s += f"{rng.choice(MINSEC):02d}"
        if rng.random() < 0.4:
            s += rng.choice(OFFSETS)
    return s, {}


def g_spaced():
    y = rng.choice(YEARS4)
    m = rng.choice(MONTHS)
    d = rng.choice(DAYS)
    return f"{y:04d} {m:02d} {d:02d}", {}


def g_fuzzy():
    inner, kw = rng.choice([g_iso(), g_slash_dash(), g_human()])
    kw = dict(kw)
    kw["fuzzy"] = True
    noise = ["meeting", "updated:", "released", "log entry", "ok", "see docs",
             "ticket", "(draft)", "hello", "see docs"]
    words = [inner] + rng.sample(noise, rng.randint(1, 3))
    rng.shuffle(words)
    return " ".join(words), kw


def g_time_only():
    h = rng.choice(list(range(0, 24)))
    mi = rng.choice(MINSEC)
    s = f"{h:02d}:{mi:02d}"
    if rng.random() < 0.6:
        s += f":{rng.choice(MINSEC):02d}"
    if rng.random() < 0.3:
        s += " " + rng.choice(["AM", "PM"])
    return s, {"default": DEF}


GENS = [g_iso, g_slash_dash, g_human, g_compact, g_spaced, g_fuzzy, g_time_only]


def run(n=5000):
    mismatch = extra = both_err = both_ok = 0
    failures = []
    for i in range(n):
        s, kw = rng.choice(GENS)()
        kw = dict(kw)
        kw.setdefault("default", DEF)
        try:
            expected = dup.parse(s, **kw)
            du_ok = True
        except Exception:
            expected = None
            du_ok = False
        try:
            got = rustdate.parse(s, **kw)
            rd_ok = True
        except ValueError:
            got = None
            rd_ok = False

        if du_ok and rd_ok:
            both_ok += 1
            if got != expected:
                mismatch += 1
                if len(failures) < 15:
                    failures.append((s, kw, expected, got))
        elif not du_ok and not rd_ok:
            both_err += 1
        elif du_ok and not rd_ok:
            mismatch += 1
            if len(failures) < 15:
                failures.append((s, kw, expected, "rustdate ERRORED"))
        else:
            extra += 1

    print(f"fuzz: {n} cases")
    print(f"  both-ok (equal) : {both_ok - mismatch}")
    print(f"  both-error      : {both_err}")
    print(f"  rustdate-extra  : {extra} (dateutil can't, we can)")
    print(f"  MISMATCHES      : {mismatch}")
    for s, kw, e, g in failures:
        print(f"    {s!r} {kw}\n      dateutil={e!r}\n      rustdate={g!r}")
    return mismatch


if __name__ == "__main__":
    sys.exit(1 if run() else 0)
