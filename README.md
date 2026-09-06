# rustdate ⚡

**Rust-powered replacement for the slow parts of `dateutil.parser`** — a PyO3 extension for Python 3.12 on Windows.

`dateutil.parser.parse` is pure Python and costs **~40 µs per string**. `rustdate.parse` does the
same job in **~0.69 µs** (**~58x speedup**), and `rustdate.parse_many` parses a whole list in
parallel (GIL released + rayon, all 12 threads) at **~0.36 µs/string** (**~112x speedup**) —
with identical output (naive/aware semantics, US month-first slash dates, AM/PM, fractional
seconds, named zones) verified against dateutil.

## Usage

```python
import rustdate

rustdate.parse("2026-09-06T14:23:45Z")        # datetime(..., tzinfo=timezone.utc)
rustdate.parse("Sep 6 2026 2:23 PM")          # naive datetime(2026, 9, 6, 14, 23)
rustdate.parse("06/09/2026 14:23:45")         # US default: month-first, like dateutil
rustdate.parse("06/09/2026", dayfirst=True)   # D/M/Y, like dateutil(dayfirst=True)
rustdate.parse("2026-09-06 14:23:45+05:30")   # aware datetime
rustdate.parse_many(list_of_strings)          # parallel batch; raises on the first bad string
```

Invalid input raises `ValueError` (dateutil raises `ParserError`, a `ValueError` subclass — so
existing `except ValueError` code keeps working).

## Build

```bash
cd rustdate
cargo test --release          # unit tests (4)
cargo build --release
cp target/release/rustdate.dll rustdate.pyd   # cdylib -> Python extension
python bench.py               # correctness vs dateutil + benchmark
```

Rust host toolchain is `x86_64-pc-windows-gnu` — no MSVC Build Tools needed.

## Supported formats

| Layout | Examples |
|---|---|
| ISO 8601 | `2026-09-06`, `2026-09-06T14:23`, `2026-09-06T14:23:45.123456Z` |
| ISO + offset | `...+05:30`, `...+0530`, `...-07:00`, `Z` |
| US slash / dash | `06/09/2026 14:23:45`, `12-25-2026 8:05 AM` (`dayfirst=True` flips order) |
| Human | `Sep 6 2026`, `Sep 6, 2026`, `6 Sep 2026` (+ optional time) |
| HTTP / email | `Sun, 06 Sep 2026 14:23:45 GMT` (weekday skipped) |
| Named zones | GMT/UTC/Z/UT, EST/EDT/CST/CDT/MST/MDT/PST/PDT, IST (+05:30) |

## Known limitations (week roadmap)

- [x] **Parallel `parse_many`**: parse to `Parsed` structs with the GIL released + rayon
      — **112x vs dateutil** on the 10k benchmark
- [x] `dayfirst=True` flag for slash/dash dates (with dateutil's >12 correction)
- [ ] `yearfirst=True` flag
- [ ] Fuzzy token matching (`fuzzy=True`) and ignored tokens
- [ ] More tz abbreviations via `tzname` table; interop with `zoneinfo`
- [ ] Package properly with `maturin` (wheel) instead of the manual `.pyd` copy
- [ ] Property-based tests (hypothesis) comparing against dateutil on random inputs

Not planned: time-only strings (dateutil resolves them to *today* — non-deterministic),
`pytz`-style localtime inference.
