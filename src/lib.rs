//! rustdate — a fast Rust port of the common paths of `dateutil.parser.parse`.
//!
//! Drop-in style API:
//!   >>> import rustdate
//!   >>> rustdate.parse("2026-09-06T14:23:45Z")
//!   datetime.datetime(2026, 9, 6, 14, 23, 45, tzinfo=datetime.timezone.utc)
//!
//! Supported layouts (the hot real-world formats):
//!   ISO:      YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][Z|±HH[:MM[:SS]]|±HHMM|named]
//!   US slash: M/D/YYYY[ H:MM[:SS[ AM|PM]]]
//!   Human:    "Sep 6 2026", "Sep 6, 2026", "6 Sep 2026" [+ time]
//!   HTTP:     "Sun, 06 Sep 2026 14:23:45 GMT" (leading weekday skipped)
//! Named zones: Z/GMT/UTC/UT, EST/EDT/CST/CDT/MST/MDT/PST/PDT, IST (+05:30).

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDateTime, PyDelta, PyList, PyString, PyTzInfo};
use rayon::prelude::*;

#[derive(Debug, Clone, Copy)]
struct Parsed {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
    offset: Option<i32>, // seconds east of UTC; None = naive
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(String),
    Alpha(String),
    Sep(char),
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut toks = Vec::with_capacity(s.len() / 2 + 4);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c.is_ascii_digit() {
            let mut n = String::with_capacity(8);
            n.push(c);
            while let Some(&d) = it.peek() {
                if d.is_ascii_digit() {
                    n.push(d);
                    it.next();
                } else {
                    break;
                }
            }
            toks.push(Tok::Num(n));
        } else if c.is_ascii_alphabetic() {
            let mut w = String::with_capacity(8);
            w.push(c);
            while let Some(&d) = it.peek() {
                if d.is_ascii_alphabetic() {
                    w.push(d);
                    it.next();
                } else {
                    break;
                }
            }
            toks.push(Tok::Alpha(w));
        } else if !c.is_whitespace() {
            toks.push(Tok::Sep(c));
        }
    }
    toks
}

fn as_num(t: &Tok) -> Option<&str> {
    match t {
        Tok::Num(n) => Some(n),
        _ => None,
    }
}

fn as_alpha(t: &Tok) -> Option<&str> {
    match t {
        Tok::Alpha(a) => Some(a),
        _ => None,
    }
}

fn month_from_name(w: &str) -> Option<u8> {
    if w.len() < 3 {
        return None;
    }
    match w[..3].to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

fn is_weekday(w: &str) -> bool {
    if w.len() < 3 {
        return false;
    }
    matches!(
        w[..3].to_ascii_lowercase().as_str(),
        "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun"
    )
}

fn named_tz(w: &str) -> Option<i32> {
    match w.to_ascii_uppercase().as_str() {
        "Z" | "GMT" | "UTC" | "UT" => Some(0),
        "EST" => Some(-5 * 3600),
        "EDT" => Some(-4 * 3600),
        "CST" => Some(-6 * 3600),
        "CDT" => Some(-5 * 3600),
        "MST" => Some(-7 * 3600),
        "MDT" => Some(-6 * 3600),
        "PST" => Some(-8 * 3600),
        "PDT" => Some(-7 * 3600),
        "IST" => Some(5 * 3600 + 30 * 60),
        _ => None,
    }
}

fn expect_sep(toks: &[Tok], i: &mut usize, c: char) -> Result<(), String> {
    match toks.get(*i) {
        Some(Tok::Sep(x)) if *x == c => {
            *i += 1;
            Ok(())
        }
        _ => Err(format!("expected {:?}", c)),
    }
}

fn take_u(
    toks: &[Tok],
    i: &mut usize,
    max_digits: usize,
    lo: i64,
    hi: i64,
    what: &str,
) -> Result<i64, String> {
    let n = match toks.get(*i) {
        Some(Tok::Num(n)) => n,
        _ => return Err(format!("expected {}", what)),
    };
    if n.len() > max_digits {
        return Err(format!("{}: too many digits", what));
    }
    let v: i64 = n.parse().map_err(|_| format!("bad {}", what))?;
    if v < lo || v > hi {
        return Err(format!("{} out of range: {}", what, v));
    }
    *i += 1;
    Ok(v)
}

fn take_year(toks: &[Tok], i: &mut usize) -> Result<i32, String> {
    let n = match toks.get(*i) {
        Some(Tok::Num(n)) => n,
        _ => return Err("expected year".into()),
    };
    let y: i32 = match n.len() {
        4 => n.parse().map_err(|_| "bad year".to_string())?,
        2 => {
            // dateutil convention: 0-68 -> 2000s, 69-99 -> 1900s
            let v: i32 = n.parse().map_err(|_| "bad year".to_string())?;
            if v < 70 {
                2000 + v
            } else {
                1900 + v
            }
        }
        _ => return Err("year must be 2 or 4 digits".into()),
    };
    *i += 1;
    Ok(y)
}

fn frac_micros(f: &str) -> Result<u32, String> {
    let t = &f[..f.len().min(6)];
    let mut v: u32 = t.parse().map_err(|_| "bad fractional seconds".to_string())?;
    for _ in t.len()..6 {
        v *= 10;
    }
    Ok(v)
}

/// Parse an optional signed numeric offset like +05:30, -07:00, +0530, +05.
/// Consumes tokens only when a sign is present; returns Ok(None) otherwise.
fn parse_signed_num(toks: &[Tok], i: &mut usize) -> Result<Option<i32>, String> {
    let neg = match toks.get(*i) {
        Some(Tok::Sep('+')) => false,
        Some(Tok::Sep('-')) => true,
        _ => return Ok(None),
    };
    *i += 1;
    let n = match toks.get(*i) {
        Some(Tok::Num(n)) => n.clone(),
        _ => return Err("expected digits after sign".into()),
    };
    *i += 1;
    let v: i32 = match n.len() {
        1 | 2 => {
            let hh: i32 = n.parse().map_err(|_| "bad offset".to_string())?;
            let mut mm = 0;
            let mut ss = 0;
            if matches!(toks.get(*i), Some(Tok::Sep(':'))) {
                *i += 1;
                match toks.get(*i) {
                    Some(Tok::Num(m)) => {
                        mm = m.parse().map_err(|_| "bad offset minutes".to_string())?;
                        *i += 1;
                    }
                    _ => return Err("bad offset minutes".into()),
                }
                if matches!(toks.get(*i), Some(Tok::Sep(':'))) {
                    *i += 1;
                    match toks.get(*i) {
                        Some(Tok::Num(s)) => {
                            ss = s.parse().map_err(|_| "bad offset seconds".to_string())?;
                            *i += 1;
                        }
                        _ => return Err("bad offset seconds".into()),
                    }
                }
            }
            if mm > 59 || ss > 59 {
                return Err("offset minutes/seconds out of range".into());
            }
            hh * 3600 + mm * 60 + ss
        }
        4 => {
            let hh: i32 = n[..2].parse().map_err(|_| "bad offset".to_string())?;
            let mm: i32 = n[2..].parse().map_err(|_| "bad offset".to_string())?;
            if mm > 59 {
                return Err("offset minutes out of range".into());
            }
            hh * 3600 + mm * 60
        }
        6 => {
            let hh: i32 = n[..2].parse().map_err(|_| "bad offset".to_string())?;
            let mm: i32 = n[2..4].parse().map_err(|_| "bad offset".to_string())?;
            let ss: i32 = n[4..].parse().map_err(|_| "bad offset".to_string())?;
            if mm > 59 || ss > 59 {
                return Err("offset out of range".into());
            }
            hh * 3600 + mm * 60 + ss
        }
        _ => return Err("bad offset format".into()),
    };
    Ok(Some(if neg { -v } else { v }))
}

fn parse_offset(toks: &[Tok], i: &mut usize) -> Result<Option<i32>, String> {
    match toks.get(*i) {
        None => Ok(None),
        Some(Tok::Alpha(a)) => {
            let base = named_tz(a).ok_or_else(|| format!("unknown timezone name '{}'", a))?;
            *i += 1;
            // allow correction after a named zone, e.g. "UTC+2"
            match parse_signed_num(toks, i)? {
                Some(delta) => Ok(Some(base + delta)),
                None => Ok(Some(base)),
            }
        }
        Some(Tok::Sep('+')) | Some(Tok::Sep('-')) => {
            parse_signed_num(toks, i).map(|o| o.or(Some(0)))
        }
        Some(t) => Err(format!(
            "unexpected token {:?} where timezone/offset expected",
            t
        )),
    }
}

/// Consume optional time-of-day + AM/PM + timezone starting at token `i`.
fn parse_time_and_offset(
    toks: &[Tok],
    mut i: usize,
) -> Result<(u8, u8, u8, u32, Option<i32>), String> {
    let mut hour = 0u8;
    let mut minute = 0u8;
    let mut second = 0u8;
    let mut micros = 0u32;
    let mut pm: Option<bool> = None;

    // ISO date-time separator "T"/"t" (tokenizer yields it as a single-letter Alpha)
    if let Some(a) = toks.get(i).and_then(as_alpha) {
        if (a == "T" || a == "t") && matches!(toks.get(i + 1), Some(Tok::Num(_))) {
            i += 1;
        }
    }

    if matches!(toks.get(i), Some(Tok::Num(_))) {
        hour = take_u(toks, &mut i, 2, 0, 23, "hour")? as u8;
        if matches!(toks.get(i), Some(Tok::Sep(':'))) {
            i += 1;
            minute = take_u(toks, &mut i, 2, 0, 59, "minute")? as u8;
            if matches!(toks.get(i), Some(Tok::Sep(':'))) {
                i += 1;
                second = take_u(toks, &mut i, 2, 0, 59, "second")? as u8;
                if matches!(toks.get(i), Some(Tok::Sep('.'))) {
                    if let Some(f) = toks.get(i + 1).and_then(as_num) {
                        micros = frac_micros(f)?;
                        i += 2;
                    }
                }
            }
        }
        if let Some(a) = toks.get(i).and_then(as_alpha) {
            match a.to_ascii_uppercase().as_str() {
                "AM" | "A" => {
                    pm = Some(false);
                    i += 1;
                }
                "PM" | "P" => {
                    pm = Some(true);
                    i += 1;
                }
                _ => {}
            }
        }
    }

    let offset = parse_offset(toks, &mut i)?;
    if i != toks.len() {
        return Err("unexpected trailing tokens".into());
    }

    if let Some(is_pm) = pm {
        if hour == 0 || hour > 12 {
            return Err("hour out of range for AM/PM clock".into());
        }
        hour = match (hour, is_pm) {
            (12, false) => 0,
            (12, true) => 12,
            (h, false) => h,
            (h, true) => h + 12,
        };
    }

    Ok((hour, minute, second, micros, offset))
}

fn days_in_month(y: i32, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Resolve the first two numeric date parts into (month, day).
/// Month-first by default (US, like dateutil); `dayfirst` swaps the order.
/// A value > 12 can only be a day, so correct it regardless (dateutil does too).
fn resolve_md(a: u8, b: u8, dayfirst: bool) -> Result<(u8, u8), String> {
    let (mut mo, mut d) = if dayfirst { (b, a) } else { (a, b) };
    if mo > 12 && d <= 12 {
        std::mem::swap(&mut mo, &mut d);
    }
    if mo > 12 {
        return Err(format!("month out of range: {}", mo));
    }
    Ok((mo, d))
}

fn parse_inner(s: &str, dayfirst: bool) -> Result<Parsed, String> {
    let toks_v = tokenize(s);
    let toks: &[Tok] = &toks_v;
    let mut i = 0usize;

    // Optional leading weekday ("Sun," / "Sunday") — skipped like dateutil does.
    if let Some(a) = toks.first().and_then(as_alpha) {
        if is_weekday(a) {
            i += 1;
            if matches!(toks.get(i), Some(Tok::Sep(','))) {
                i += 1;
            }
        }
    }

    let (year, month, day) = match toks.get(i) {
        Some(Tok::Num(n)) => {
            if n.len() == 4 && matches!(toks.get(i + 1), Some(Tok::Sep('-'))) {
                // ISO: YYYY-MM-DD
                let y: i32 = n.parse().map_err(|_| "bad year".to_string())?;
                i += 2;
                let mo = take_u(toks, &mut i, 2, 1, 12, "month")? as u8;
                expect_sep(toks, &mut i, '-')?;
                let d = take_u(toks, &mut i, 2, 1, 31, "day")? as u8;
                (y, mo, d)
            } else if matches!(toks.get(i + 1), Some(Tok::Sep('/'))) {
                // Slash: M/D/YYYY (dateutil default) or D/M/YYYY with dayfirst
                let a = take_u(toks, &mut i, 2, 1, 31, "date part")? as u8;
                expect_sep(toks, &mut i, '/')?;
                let b = take_u(toks, &mut i, 2, 1, 31, "date part")? as u8;
                expect_sep(toks, &mut i, '/')?;
                let y = take_year(toks, &mut i)?;
                let (mo, d) = resolve_md(a, b, dayfirst)?;
                (y, mo, d)
            } else if n.len() <= 2
                && matches!(toks.get(i + 1), Some(Tok::Sep('-')))
                && toks.get(i + 2).and_then(as_num).is_some()
            {
                // Dashed: M-D-Y (or D-M-Y with dayfirst), e.g. 06-09-2026
                let a = take_u(toks, &mut i, 2, 1, 31, "date part")? as u8;
                expect_sep(toks, &mut i, '-')?;
                let b = take_u(toks, &mut i, 2, 1, 31, "date part")? as u8;
                expect_sep(toks, &mut i, '-')?;
                let y = take_year(toks, &mut i)?;
                let (mo, d) = resolve_md(a, b, dayfirst)?;
                (y, mo, d)
            } else if n.len() <= 2
                && toks
                    .get(i + 1)
                    .and_then(as_alpha)
                    .and_then(month_from_name)
                    .is_some()
            {
                // "6 Sep 2026"
                let d = take_u(toks, &mut i, 2, 1, 31, "day")? as u8;
                let mo = toks
                    .get(i)
                    .and_then(as_alpha)
                    .and_then(month_from_name)
                    .unwrap();
                i += 1;
                let y = take_year(toks, &mut i)?;
                (y, mo, d)
            } else {
                return Err("unrecognized date layout".into());
            }
        }
        Some(Tok::Alpha(a)) => {
            // "Sep 6 2026" / "Sep 6, 2026"
            let mo = month_from_name(a).ok_or_else(|| format!("unknown month name '{}'", a))?;
            i += 1;
            let d = take_u(toks, &mut i, 2, 1, 31, "day")? as u8;
            if matches!(toks.get(i), Some(Tok::Sep(','))) {
                i += 1;
            }
            let y = take_year(toks, &mut i)?;
            (y, mo, d)
        }
        _ => return Err("unrecognized date layout".into()),
    };

    let (hour, minute, second, micros, offset) = parse_time_and_offset(toks, i)?;

    if !(1..=9999).contains(&year) {
        return Err("year out of range".into());
    }
    let dim = days_in_month(year, month);
    if day > dim {
        return Err(format!(
            "day {} out of range for month {} (year {})",
            day, month, year
        ));
    }

    Ok(Parsed {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
        offset,
    })
}

fn build_datetime<'py>(py: Python<'py>, p: Parsed) -> PyResult<Bound<'py, PyDateTime>> {
    let tz: Option<Bound<'py, PyTzInfo>> = match p.offset {
        None => None,
        Some(0) => Some(PyTzInfo::utc(py)?.to_owned()),
        Some(off) => {
            let delta = PyDelta::new(py, 0, off, 0, false)?;
            Some(PyTzInfo::fixed_offset(py, delta)?)
        }
    };
    PyDateTime::new(
        py,
        p.year,
        p.month,
        p.day,
        p.hour,
        p.minute,
        p.second,
        p.micros,
        tz.as_ref(),
    )
}

/// Parse a date/time string into a datetime.datetime.
/// Naive when no timezone is present, aware when it is (like dateutil).
#[pyfunction(signature = (s, *, dayfirst = false))]
fn parse<'py>(
    py: Python<'py>,
    s: &'py str,
    dayfirst: bool,
) -> PyResult<Bound<'py, PyDateTime>> {
    let p = parse_inner(s, dayfirst).map_err(|e| {
        PyValueError::new_err(format!("rustdate: could not parse {:?}: {}", s, e))
    })?;
    build_datetime(py, p)
}

/// Parse a list of date/time strings in parallel (GIL released during parsing);
/// fails on the first bad string in input order.
#[pyfunction(signature = (items, *, dayfirst = false))]
fn parse_many<'py>(
    py: Python<'py>,
    items: &Bound<'py, PyList>,
    dayfirst: bool,
) -> PyResult<Vec<Bound<'py, PyDateTime>>> {
    let mut strings = Vec::with_capacity(items.len());
    for item in items.iter() {
        let s = item.downcast::<PyString>()?;
        strings.push(s.to_str()?.to_string());
    }
    let parsed: Vec<Result<Parsed, String>> = py.allow_threads(|| {
        strings
            .par_iter()
            .map(|s| parse_inner(s, dayfirst))
            .collect::<Vec<_>>()
    });
    let mut out = Vec::with_capacity(parsed.len());
    for (s, r) in strings.iter().zip(parsed) {
        match r {
            Ok(p) => out.push(build_datetime(py, p)?),
            Err(e) => {
                return Err(PyValueError::new_err(format!(
                    "rustdate: could not parse {:?}: {}",
                    s, e
                )))
            }
        }
    }
    Ok(out)
}

#[pymodule]
fn rustdate(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse, m)?)?;
    m.add_function(wrap_pyfunction!(parse_many, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Parsed {
        parse_inner(s, false).unwrap_or_else(|e| panic!("{}: {}", s, e))
    }

    fn p_df(s: &str, dayfirst: bool) -> Parsed {
        parse_inner(s, dayfirst).unwrap_or_else(|e| panic!("{}: {}", s, e))
    }

    #[test]
    fn iso() {
        let x = p("2026-09-06T14:23:45Z");
        assert_eq!((x.year, x.month, x.day, x.hour, x.minute, x.second), (2026, 9, 6, 14, 23, 45));
        assert_eq!(x.offset, Some(0));
        let x = p("2026-09-06 14:23:45+05:30");
        assert_eq!(x.offset, Some(19800));
        let x = p("2026-09-06T14:23:45.123456-07:00");
        assert_eq!(x.micros, 123456);
        assert_eq!(x.offset, Some(-25200));
        let x = p("2026-09-06T14:23");
        assert_eq!((x.hour, x.minute, x.second), (14, 23, 0));
        assert_eq!(x.offset, None);
    }

    #[test]
    fn human() {
        let x = p("Sep 6 2026 2:23 PM");
        assert_eq!((x.month, x.day, x.year, x.hour, x.minute), (9, 6, 2026, 14, 23));
        let x = p("6 Sep 2026");
        assert_eq!((x.month, x.day, x.year, x.hour), (9, 6, 2026, 0));
        let x = p("Sun, 06 Sep 2026 14:23:45 GMT");
        assert_eq!(x.offset, Some(0));
    }

    #[test]
    fn slash() {
        let x = p("06/09/2026 14:23:45");
        assert_eq!((x.month, x.day, x.year, x.hour), (6, 9, 2026, 14));
        let x = p("12/25/26 8:05 AM");
        assert_eq!((x.month, x.day, x.year, x.hour), (12, 25, 2026, 8));
    }

    #[test]
    fn dayfirst_and_dashed() {
        // default month-first, with dateutil's >12 correction
        let x = p("25/12/2026");
        assert_eq!((x.month, x.day, x.year), (12, 25, 2026));
        let x = p("13/05/2026");
        assert_eq!((x.month, x.day), (5, 13));
        // dayfirst flips slash order
        let x = p_df("06/09/2026", true);
        assert_eq!((x.month, x.day, x.year), (9, 6, 2026));
        // dashed M-D-Y
        let x = p("12-25-2026 8:05");
        assert_eq!((x.month, x.day, x.year, x.hour), (12, 25, 2026, 8));
        let x = p_df("25-12-2026", true);
        assert_eq!((x.month, x.day), (12, 25));
    }

    #[test]
    fn rejects() {
        for bad in ["", "garbage", "2026-13-40", "2026-02-30", "25/13/2026", "Sep 40 2026"] {
            assert!(parse_inner(bad, false).is_err(), "should reject {:?}", bad);
        }
    }
}
