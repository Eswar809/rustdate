//! rustdate — a fast, dateutil-compatible date parser written in Rust (PyO3).
//!
//! API mirrors `dateutil.parser.parse` for the kwargs that matter:
//!   >>> import rustdate
//!   >>> rustdate.parse("2026-09-06T14:23:45Z")
//!   >>> rustdate.parse("Sep 6", default=datetime(2020, 3, 15))   # missing fields from default
//!   >>> rustdate.parse("I met him on Sep 6 2026 at 2 PM", fuzzy=True)
//!   >>> rustdate.parse("06/09/2026", dayfirst=True, yearfirst=False)
//!   >>> rustdate.parse_many(list_of_strings, fuzzy=True)
//!
//! All parse failures raise `rustdate.ParserError` (a ValueError subclass, like
//! dateutil's `dateutil.parser.ParserError`).
//!
//! Supported layouts:
//!   ISO 8601:      YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][Z|±HH[:MM[:SS]]|±HHMM]
//!   ISO week:      2026-W36-6          (optional weekday; otherwise rejected)
//!   ISO ordinal:   2026-249            (day-of-year)
//!   Compact ISO:   20260906[T[hhmm|hhmmss]]
//!   Spaced YMD:    2026 09 06
//!   US slash/dash: M/D/Y, M-D-Y (2 or 3 parts; dayfirst/yearfirst aware)
//!   Human:         "Sep 6 2026", "Sep 6, 2026", "6 Sep 2026", "Sep 2026", "Sep 6"
//!   HTTP/email:    "Sun, 06 Sep 2026 14:23:45 GMT" (leading weekday skipped)
//!   Time-only:     "14:23:45", "2:23 PM" (date filled from `default`)
//!   Jump words:    at/on/of/and/ad + ordinal suffixes st/nd/rd/th always ignored
//!   12h clock:     2:23 PM, 2:23 p.m., 2 PM
//! Timezones:     Z/GMT/UTC/UT, EST/EDT/CST/CDT/MST/MDT/PST/PDT, IST (+05:30) as
//!                fixed offsets; anything else is resolved via `zoneinfo` (IANA
//!                names like "Asia/Kolkata", legacy keys like "EST5EDT" with DST).
//! Missing components are copied from `default` (default: today at midnight),
//! exactly like dateutil.

use pyo3::create_exception;
use pyo3::prelude::*;
use pyo3::types::{PyDateTime, PyDelta, PyList, PyString, PyTzInfo};
use rayon::prelude::*;

create_exception!(
    rustdate,
    ParserError,
    pyo3::exceptions::PyValueError,
    "dateutil-compatible parse error (subclass of ValueError)"
);

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

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

/// Merge IANA zone-path fragments ("Asia" "/" "Kolkata", "New" "_" "York")
/// into single Alpha tokens so they can be resolved via zoneinfo as one name.
fn merge_zone_paths(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut idx = 0usize;
    while idx < toks.len() {
        match &toks[idx] {
            Tok::Alpha(first) => {
                let mut acc = first.clone();
                let mut k = idx + 1;
                while k + 1 < toks.len()
                    && matches!(toks[k], Tok::Sep('/') | Tok::Sep('_'))
                    && matches!(toks[k + 1], Tok::Alpha(_))
                {
                    if let (Tok::Sep(c), Tok::Alpha(w)) = (&toks[k], &toks[k + 1]) {
                        acc.push(*c);
                        acc.push_str(w);
                    }
                    k += 2;
                }
                out.push(Tok::Alpha(acc));
                idx = k;
            }
            other => {
                out.push(other.clone());
                idx += 1;
            }
        }
    }
    out
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

/// Words dateutil always "jumps" (ignores), even without fuzzy=True.
fn is_jump_word(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "at" | "on" | "of" | "and" | "ad" | "st" | "nd" | "rd" | "th"
    )
}

fn drop_jumps(toks: Vec<Tok>) -> Vec<Tok> {
    toks.into_iter()
        .filter(|t| match t {
            Tok::Alpha(a) => !is_jump_word(a),
            _ => true,
        })
        .collect()
}

/// fuzzy=True: keep only tokens that could belong to a date/time, drop the rest.
fn filter_fuzzy(toks: Vec<Tok>) -> Vec<Tok> {
    let n = toks.len();
    let mut keep = vec![false; n];
    for (k, t) in toks.iter().enumerate() {
        match t {
            Tok::Num(_) => keep[k] = true,
            Tok::Sep(c) => keep[k] = matches!(c, '-' | '+' | '/' | ':' | ',' | '.'),
            Tok::Alpha(a) => {
                let lo = a.to_ascii_lowercase();
                keep[k] = month_from_name(a).is_some()
                    || is_weekday(a)
                    || tz_utc(a).is_some() || is_tz_abbrev(a)
                    || a.contains('/') || a.contains('_')
                    || matches!(lo.as_str(), "am" | "pm")
                    // single letters only make sense glued to a number:
                    // T (ISO sep), W (ISO week), a/p (12h clock), z (UTC)
                    || (matches!(lo.as_str(), "a" | "p" | "z" | "t" | "w")
                        && ((k > 0 && matches!(toks[k - 1], Tok::Num(_)))
                            || (k + 1 < n && matches!(toks[k + 1], Tok::Num(_)))));
            }
        }
    }
    // IANA paths like Asia/Kolkata: keep alpha/slash/alpha chains
    for k in 1..n.saturating_sub(1) {
        if toks[k] == Tok::Sep('/') {
            if matches!(toks[k - 1], Tok::Alpha(_)) && matches!(toks[k + 1], Tok::Alpha(_)) {
                keep[k - 1] = true;
                keep[k] = true;
                keep[k + 1] = true;
            }
        }
    }
    let kept: Vec<Tok> = toks
        .into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(t, _)| t)
        .collect();
    // a kept separator must sit between two kept tokens, otherwise it is
    // punctuation debris ("updated: 2023-05-22", "12-28 06:19 updated:")
    let mut out: Vec<Tok> = Vec::with_capacity(kept.len());
    for (idx, t) in kept.iter().enumerate() {
        if matches!(t, Tok::Sep(_)) {
            let prev_ok = idx > 0 && !matches!(kept[idx - 1], Tok::Sep(_));
            let next_ok = idx + 1 < kept.len() && !matches!(kept[idx + 1], Tok::Sep(_));
            if !(prev_ok && next_ok) {
                continue;
            }
        }
        out.push(t.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Small lookups
// ---------------------------------------------------------------------------

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

/// Zone names dateutil understands by default → aware result.
fn tz_utc(w: &str) -> Option<i32> {
    match w.to_ascii_uppercase().as_str() {
        "Z" | "GMT" | "UTC" | "UT" => Some(0),
        _ => None,
    }
}

/// Zone abbreviations dateutil recognizes but cannot resolve by default
/// ("identified but not understood"): the token is dropped, result naive —
/// exactly what dateutil does without a `tzinfos` argument. For aware
/// results, use IANA names ("Asia/Kolkata") or "EST5EDT" style zones.
fn is_tz_abbrev(w: &str) -> bool {
    matches!(
        w.to_ascii_uppercase().as_str(),
        "EST" | "EDT" | "CST" | "CDT" | "MST" | "MDT" | "PST" | "PDT" | "IST"
    )
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

fn two_digit_year(v: i64) -> i64 {
    if v < 70 {
        2000 + v
    } else {
        1900 + v
    }
}

// ---------------------------------------------------------------------------
// Calendar math (Howard Hinnant's civil algorithms — no chrono dependency)
// ---------------------------------------------------------------------------

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + if m <= 2 { 1 } else { 0 }) as i32, m, d)
}

/// ISO weekday for a day number, Monday=1..Sunday=7 (1970-01-01 was a Thursday).
fn iso_weekday(z: i64) -> u32 {
    (((z % 7 + 7) % 7 + 3) % 7 + 1) as u32
}

fn iso_week_to_date(y: i32, week: u32, wd: u32) -> Result<(i32, u8, u8), String> {
    let jan4 = days_from_civil(y, 1, 4);
    let week1_mon = jan4 - (iso_weekday(jan4) as i64 - 1);
    let target = week1_mon + (week as i64 - 1) * 7 + (wd as i64 - 1);
    let (yy, m, d) = civil_from_days(target);
    Ok((yy, m as u8, d as u8))
}

fn ordinal_to_month_day(y: i32, doy: u32) -> Result<(u8, u8), String> {
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let max = if leap { 366 } else { 365 };
    if doy < 1 || doy > max {
        return Err(format!("ordinal day {} out of range for {}", doy, y));
    }
    if leap && doy == 60 {
        return Ok((2, 29));
    }
    let eff = if leap && doy > 60 { doy - 1 } else { doy };
    const CUM: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    for m in (1..=12).rev() {
        if eff > CUM[m as usize - 1] {
            return Ok((m as u8, (eff - CUM[m as usize - 1]) as u8));
        }
    }
    Err("bad ordinal".into())
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

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

fn year_from_str(n: &str) -> Result<i32, String> {
    match n.len() {
        4 => n.parse::<i32>().map_err(|_| "bad year".to_string()),
        2 => {
            let v: i64 = n.parse().map_err(|_| "bad year".to_string())?;
            Ok(two_digit_year(v) as i32)
        }
        _ => Err("year must be 2 or 4 digits".into()),
    }
}

fn frac_micros(f: &str) -> Result<u32, String> {
    let t = &f[..f.len().min(6)];
    let mut v: u32 = t.parse().map_err(|_| "bad fractional seconds".to_string())?;
    for _ in t.len()..6 {
        v *= 10;
    }
    Ok(v)
}

/// Numeric offset like +05:30, -07:00, +0530, +05 (sign already present).
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

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
enum TzSpec {
    #[default]
    None,
    Offset(i32),
    Named(String), // resolved via zoneinfo at build time (DST-aware)
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Fields {
    year: Option<i32>,
    month: Option<u8>,
    day: Option<u8>,
    hour: Option<u8>,
    minute: Option<u8>,
    second: Option<u8>,
    micros: Option<u32>,
    tz: TzSpec,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Filled {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct ParseOpts {
    dayfirst: bool,
    yearfirst: bool,
    fuzzy: bool,
}

#[derive(Debug, Clone, Copy)]
struct DefaultNum {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
}

const ZEROS: DefaultNum = DefaultNum {
    year: 2020,
    month: 1,
    day: 1,
    hour: 0,
    minute: 0,
    second: 0,
    micros: 0,
};

fn fill(f: &Fields, d: &DefaultNum) -> Result<Filled, String> {
    let year = f.year.unwrap_or(d.year);
    let month = f.month.unwrap_or(d.month);
    let day = f.day.unwrap_or(d.day);
    let hour = f.hour.unwrap_or(d.hour);
    let minute = f.minute.unwrap_or(d.minute);
    let second = f.second.unwrap_or(d.second);
    let micros = f.micros.unwrap_or(d.micros);
    if !(1..=9999).contains(&year) {
        return Err(format!("year out of range: {}", year));
    }
    if month == 0 || month > 12 {
        return Err(format!("month out of range: {}", month));
    }
    let dim = days_in_month(year, month);
    if day == 0 || day > dim {
        return Err(format!(
            "day {} out of range for month {} (year {})",
            day, month, year
        ));
    }
    if hour > 23 {
        return Err(format!("hour out of range: {}", hour));
    }
    if minute > 59 {
        return Err(format!("minute out of range: {}", minute));
    }
    if second > 59 {
        return Err(format!("second out of range: {}", second));
    }
    Ok(Filled {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
    })
}

// ---------------------------------------------------------------------------
// Date-part resolution
// ---------------------------------------------------------------------------

/// Two remaining parts after the year is fixed. dayfirst prefers (d, m)
/// but falls back to (m, d) when the swapped month would be impossible:
/// "2024/04/14" + dayfirst -> 2024-04-14 (14 cannot be a month).
fn md_with_fallback(a: i64, b: i64, dayfirst: bool) -> Result<(u8, u8), String> {
    let cands = if dayfirst { [(b, a), (a, b)] } else { [(a, b), (b, a)] };
    for (m, d) in cands {
        if (1..=12).contains(&m) && (1..=31).contains(&d) {
            return Ok((m as u8, d as u8));
        }
    }
    Err(format!("unresolvable date parts ({}, {})", a, b))
}

fn try_ymd(y: i64, m: i64, d: i64) -> Option<(i32, u8, u8)> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if y < 100 { two_digit_year(y) } else { y };
    if !(1..=9999).contains(&y) {
        return None;
    }
    Some((y as i32, m as u8, d as u8))
}

/// Three numeric parts. p1/p3 may carry a 4-digit year; p2 is 1-2 digits.
/// Tries role assignments in dateutil's preference order until one is valid,
/// so "01-22-15" + yearfirst resolves to 2015-01-22 (month 22 is impossible).
fn resolve_ymd3(
    p1: i64,
    p1len: usize,
    p2: i64,
    p3: i64,
    p3len: usize,
    opts: &ParseOpts,
) -> Result<(i32, u8, u8), String> {
    let mut cands: Vec<(i64, i64, i64)> = Vec::with_capacity(4);
    if p1len == 4 {
        if opts.dayfirst {
            cands.push((p1, p3, p2));
        }
        cands.push((p1, p2, p3));
        if !opts.dayfirst {
            cands.push((p1, p3, p2));
        }
    } else if p3len == 4 {
        if opts.dayfirst {
            cands.push((p3, p2, p1));
        }
        cands.push((p3, p1, p2));
        if !opts.dayfirst {
            cands.push((p3, p2, p1));
        }
    } else if opts.yearfirst {
        // yearfirst pins the year to p1, unless p2 cannot be a month at all
        // ("03-27-01" + yearfirst -> 2001-03-27, like dateutil)
        if opts.dayfirst {
            if p2 > 12 {
                cands.push((p3, p1, p2));
            } else {
                cands.push((p1, p3, p2));
            }
            cands.push((p1, p2, p3));
            cands.push((p3, p1, p2));
            cands.push((p3, p2, p1));
        } else {
            if p2 > 12 {
                cands.push((p3, p1, p2));
            } else {
                cands.push((p1, p2, p3));
            }
            cands.push((p1, p3, p2));
            cands.push((p3, p1, p2));
            cands.push((p3, p2, p1));
        }
    } else if opts.dayfirst {
        cands.push((p3, p2, p1));
        cands.push((p3, p1, p2));
        cands.push((p1, p3, p2));
        cands.push((p1, p2, p3));
    } else {
        cands.push((p3, p1, p2));
        cands.push((p3, p2, p1));
        cands.push((p1, p2, p3));
        cands.push((p1, p3, p2));
    }
    for (y, m, d) in cands {
        // dateutil applies the >12 correction inside each candidate before
        // rejecting it: "12-08-24" + dayfirst+yearfirst -> 2012-08-24
        let (mut m, mut d) = (m, d);
        if m > 12 && d <= 12 {
            std::mem::swap(&mut m, &mut d);
        }
        if let Some(x) = try_ymd(y, m, d) {
            return Ok(x);
        }
    }
    Err(format!("unresolvable date parts ({}, {}, {})", p1, p2, p3))
}

/// Two numeric parts: Y/M, M/Y or M/D (no year).
fn resolve_ymd2(
    p1: i64,
    p1len: usize,
    p2: i64,
    p2len: usize,
    opts: &ParseOpts,
) -> Result<(Option<i32>, u8, Option<u8>), String> {
    if p1len == 4 {
        if !(1..=12).contains(&p2) {
            return Err(format!("month out of range: {}", p2));
        }
        return Ok((Some(p1 as i32), p2 as u8, None));
    }
    if p2len == 4 {
        if !(1..=12).contains(&p1) {
            return Err(format!("month out of range: {}", p1));
        }
        return Ok((Some(p2 as i32), p1 as u8, None));
    }
    // a value > 31 can only be a year: "99/11" -> 1999-11
    if p1 > 31 {
        if !(1..=12).contains(&p2) {
            return Err(format!("month out of range: {}", p2));
        }
        return Ok((Some(two_digit_year(p1) as i32), p2 as u8, None));
    }
    if p2 > 31 {
        if !(1..=12).contains(&p1) {
            return Err(format!("month out of range: {}", p1));
        }
        return Ok((Some(two_digit_year(p2) as i32), p1 as u8, None));
    }
    let (mut m, mut d) = if opts.dayfirst { (p2, p1) } else { (p1, p2) };
    if m > 12 && d <= 12 {
        std::mem::swap(&mut m, &mut d);
    }
    if !(1..=12).contains(&m) {
        return Err(format!("month out of range: {}", m));
    }
    if d == 0 {
        return Err("day out of range: 0".into());
    }
    Ok((None, m as u8, Some(d as u8)))
}

// ---------------------------------------------------------------------------
// Time + timezone
// ---------------------------------------------------------------------------

fn parse_time_and_offset(
    toks: &[Tok],
    mut i: usize,
    has_date: bool,
) -> Result<(Option<u8>, Option<u8>, Option<u8>, Option<u32>, TzSpec), String> {
    let mut hour = None;
    let mut minute = None;
    let mut second = None;
    let mut micros = None;
    let mut pm: Option<bool> = None;

    // ISO date-time separator "T"/"t" (single-letter Alpha after tokenizing)
    if let Some(a) = toks.get(i).and_then(as_alpha) {
        if a.len() == 1 && a.eq_ignore_ascii_case("t") && matches!(toks.get(i + 1), Some(Tok::Num(_)))
        {
            i += 1;
        }
    }

    if let Some(Tok::Num(n)) = toks.get(i) {
        let n = n.clone();
        i += 1;
        match n.len() {
            6 => {
                // compact hhmmss (after "T" or in time position)
                let h: u8 = n[0..2].parse().map_err(|_| "bad hour".to_string())?;
                let m: u8 = n[2..4].parse().map_err(|_| "bad minute".to_string())?;
                let s: u8 = n[4..6].parse().map_err(|_| "bad second".to_string())?;
                if h > 23 || m > 59 || s > 59 {
                    return Err("compact time out of range".into());
                }
                hour = Some(h);
                minute = Some(m);
                second = Some(s);
            }
            4 => {
                // compact hhmm (after "T" or in time position)
                let h: u8 = n[0..2].parse().map_err(|_| "bad hour".to_string())?;
                let m: u8 = n[2..4].parse().map_err(|_| "bad minute".to_string())?;
                if h > 23 || m > 59 {
                    return Err("compact time out of range".into());
                }
                hour = Some(h);
                minute = Some(m);
            }
            _ => {
                if n.len() > 2 {
                    return Err(format!("bad hour: {}", n));
                }
                let h: u8 = n.parse().map_err(|_| "bad hour".to_string())?;
                if h > 23 {
                    return Err(format!("hour out of range: {}", h));
                }
                hour = Some(h);
                if matches!(toks.get(i), Some(Tok::Sep(':'))) {
                    i += 1;
                    minute = Some(take_u(toks, &mut i, 2, 0, 59, "minute")? as u8);
                    if matches!(toks.get(i), Some(Tok::Sep(':'))) {
                        i += 1;
                        second = Some(take_u(toks, &mut i, 2, 0, 59, "second")? as u8);
                        if matches!(toks.get(i), Some(Tok::Sep('.'))) {
                            if let Some(f) = toks.get(i + 1).and_then(as_num) {
                                micros = Some(frac_micros(f)?);
                                i += 2;
                            }
                        }
                    }
                }
            }
        }
        // AM/PM (also "p.m." / "a.m.")
        if let Some(a) = toks.get(i).and_then(as_alpha) {
            let up = a.to_ascii_uppercase();
            if up == "AM" || up == "A" || up == "PM" || up == "P" {
                pm = Some(up.starts_with('P'));
                i += 1;
                if matches!(toks.get(i), Some(Tok::Sep('.'))) {
                    if let Some(Tok::Alpha(m2)) = toks.get(i + 1) {
                        if m2.eq_ignore_ascii_case("m") {
                            i += 2;
                        }
                    }
                }
            }
        }
        if let Some(is_pm) = pm {
            // dateutil: 12 AM -> 0, 0 PM -> 12; hour > 12 with an AM/PM marker
            // raises for time-only strings but is ignored when a date exists
            let h = hour.ok_or("AM/PM without an hour")?;
            if h > 12 {
                if !has_date {
                    return Err("hour out of range for AM/PM clock".into());
                }
            } else {
                hour = Some(if is_pm {
                    (h % 12) + 12
                } else if h == 12 {
                    0
                } else {
                    h
                });
            }
        }
    }

    let tz = parse_offset(toks, &mut i)?;
    if i != toks.len() {
        return Err("unexpected trailing tokens".into());
    }
    Ok((hour, minute, second, micros, tz))
}

fn parse_offset(toks: &[Tok], i: &mut usize) -> Result<TzSpec, String> {
    match toks.get(*i) {
        None => Ok(TzSpec::None),
        // trailing sentence punctuation: "Sep 6 2026 2 PM." / "...2026-09-06, thanks"
        Some(Tok::Sep('.')) | Some(Tok::Sep(',')) => {
            *i += 1;
            Ok(TzSpec::None)
        }
        Some(Tok::Alpha(a)) => {
            // Zone names with an embedded offset ("EST5EDT", "PST8PDT") get
            // split by the tokenizer into Alpha+Num+Alpha — reassemble them.
            if let (Some(Tok::Num(m)), Some(Tok::Alpha(t2))) =
                (toks.get(*i + 1), toks.get(*i + 2))
            {
                if is_tz_abbrev(a) && is_tz_abbrev(t2) && m.len() <= 2 {
                    let full = format!("{}{}{}", a, m, t2);
                    *i += 3;
                    return Ok(TzSpec::Named(full));
                }
            }
            if let Some(base) = tz_utc(a) {
                *i += 1;
                match parse_signed_num(toks, i)? {
                    Some(delta) => Ok(TzSpec::Offset(base + delta)),
                    None => Ok(TzSpec::Offset(base)),
                }
            } else if is_tz_abbrev(a) {
                // dateutil default: recognized, dropped → naive result
                *i += 1;
                Ok(TzSpec::None)
            } else {
                let name = a.clone();
                *i += 1;
                if parse_signed_num(toks, i)?.is_some() {
                    return Err("offset correction after a zone name is not supported".into());
                }
                Ok(TzSpec::Named(name))
            }
        }
        Some(Tok::Sep('+')) | Some(Tok::Sep('-')) => {
            Ok(TzSpec::Offset(parse_signed_num(toks, i)?.unwrap_or(0)))
        }
        Some(t) => Err(format!(
            "unexpected token {:?} where timezone/offset expected",
            t
        )),
    }
}

// ---------------------------------------------------------------------------
// Main parser
// ---------------------------------------------------------------------------

fn parse_inner(s: &str, opts: &ParseOpts) -> Result<Fields, String> {
    let toks_v = tokenize(s);
    let toks_v = drop_jumps(toks_v);
    let toks_v = merge_zone_paths(toks_v);
    let toks_v = if opts.fuzzy {
        filter_fuzzy(toks_v)
    } else {
        toks_v
    };
    let toks: &[Tok] = &toks_v;
    let mut i = 0usize;

    if toks.is_empty() {
        return Err("no date information found".into());
    }

    // Optional leading weekday ("Sun," / "Sunday") — skipped like dateutil does.
    if let Some(a) = toks.first().and_then(as_alpha) {
        if is_weekday(a) {
            i += 1;
            if matches!(toks.get(i), Some(Tok::Sep(','))) {
                i += 1;
            }
        }
    }

    let mut f = Fields::default();

    match toks.get(i) {
        Some(Tok::Num(n0)) => {
            let n0 = n0.clone();
            if n0.len() == 8 {
                // compact yyyymmdd
                f.year = Some(n0[..4].parse().map_err(|_| "bad year".to_string())?);
                f.month = Some(n0[4..6].parse().map_err(|_| "bad month".to_string())?);
                f.day = Some(n0[6..8].parse().map_err(|_| "bad day".to_string())?);
                i += 1;
            } else if n0.len() == 4 && matches!(toks.get(i + 1), Some(Tok::Sep('-'))) {
                // 4-digit year + '-' → ISO family: month-day / week / ordinal
                let y: i32 = n0.parse().map_err(|_| "bad year".to_string())?;
                f.year = Some(y);
                i += 2;
                match toks.get(i) {
                    Some(Tok::Alpha(a))
                        if a.len() == 1 && a.eq_ignore_ascii_case("w") =>
                    {
                        i += 1;
                        let week = take_u(toks, &mut i, 2, 1, 53, "ISO week")? as u32;
                        if !matches!(toks.get(i), Some(Tok::Sep('-'))) {
                            return Err(
                                "ISO week date requires a weekday (e.g. 2026-W36-6)".into()
                            );
                        }
                        i += 1;
                        let wd = take_u(toks, &mut i, 1, 1, 7, "ISO weekday")? as u32;
                        let (wy, m, d) = iso_week_to_date(y, week, wd)?;
                        f.year = Some(wy);
                        f.month = Some(m);
                        f.day = Some(d);
                    }
                    Some(Tok::Num(n1)) if n1.len() == 3 => {
                        let doy: u32 = n1.parse().map_err(|_| "bad ordinal day".to_string())?;
                        let (m, d) = ordinal_to_month_day(y, doy)?;
                        f.month = Some(m);
                        f.day = Some(d);
                        i += 1;
                    }
                    Some(Tok::Num(_)) => {
                        // month-day (dayfirst swaps them, like dateutil does
                        // even for ISO dates: "2026-09-06" + dayfirst -> Jun 9)
                        let a = take_u(toks, &mut i, 2, 1, 31, "date part")?;
                        if matches!(toks.get(i), Some(Tok::Sep('-'))) {
                            i += 1;
                            let b = take_u(toks, &mut i, 2, 1, 31, "date part")?;
                            let (mo, dy) = md_with_fallback(a, b, opts.dayfirst)?;
                            f.month = Some(mo);
                            f.day = Some(dy);
                        } else {
                            // day is optional: "2026-09" means year-month
                            if a > 12 {
                                return Err(format!("month out of range: {}", a));
                            }
                            f.month = Some(a as u8);
                        }
                    }
                    None => {
                        // year only — month/day come from default
                    }
                    _ => return Err("unrecognized date after year".into()),
                }
            } else if n0.len() == 4 && matches!(toks.get(i + 1), Some(Tok::Sep('/'))) {
                // 2026/09/06 or 2026/09 (dayfirst swaps month/day)
                let y: i32 = n0.parse().map_err(|_| "bad year".to_string())?;
                f.year = Some(y);
                i += 2;
                let a = take_u(toks, &mut i, 2, 1, 31, "date part")?;
                if matches!(toks.get(i), Some(Tok::Sep('/'))) {
                    i += 1;
                    let b = take_u(toks, &mut i, 2, 1, 31, "date part")?;
                    let (mo, dy) = md_with_fallback(a, b, opts.dayfirst)?;
                    f.month = Some(mo);
                    f.day = Some(dy);
                } else {
                    if a > 12 {
                        return Err(format!("month out of range: {}", a));
                    }
                    f.month = Some(a as u8);
                }
            } else if n0.len() == 4
                && toks.get(i + 1).and_then(as_num).map(|n| n.len() <= 2).unwrap_or(false)
                && toks.get(i + 2).and_then(as_num).map(|n| n.len() <= 2).unwrap_or(false)
            {
                // spaced Y M D: "2026 09 06"
                let y: i32 = n0.parse().map_err(|_| "bad year".to_string())?;
                f.year = Some(y);
                i += 1;
                let mo = take_u(toks, &mut i, 2, 1, 12, "month")? as u8;
                let d = take_u(toks, &mut i, 2, 1, 31, "day")? as u8;
                f.month = Some(mo);
                f.day = Some(d);
            } else if n0.len() == 4
                && toks.get(i + 1).and_then(as_num).map(|n| n.len() <= 2).unwrap_or(false)
                && (toks.get(i + 2).is_none() || matches!(toks.get(i + 2), Some(Tok::Sep(','))))
            {
                // "2026 09" — year and month only
                let y: i32 = n0.parse().map_err(|_| "bad year".to_string())?;
                f.year = Some(y);
                i += 1;
                let mo = take_u(toks, &mut i, 2, 1, 12, "month")? as u8;
                f.month = Some(mo);
            } else if n0.len() <= 2 && matches!(toks.get(i + 1), Some(Tok::Sep(':'))) {
                // time-only — the whole date comes from default
            } else if n0.len() <= 2 && matches!(toks.get(i + 1), Some(Tok::Sep('/'))) {
                let p1: i64 = n0.parse().map_err(|_| "bad date part".to_string())?;
                let p1len = n0.len();
                i += 2;
                let n1 = match toks.get(i) {
                    Some(Tok::Num(x)) => x.clone(),
                    _ => return Err("expected date part after '/'".into()),
                };
                i += 1;
                let p2: i64 = n1.parse().map_err(|_| "bad date part".to_string())?;
                if matches!(toks.get(i), Some(Tok::Sep('/'))) {
                    i += 1;
                    let n2 = match toks.get(i) {
                        Some(Tok::Num(x)) => x.clone(),
                        _ => return Err("expected year after '/'".into()),
                    };
                    i += 1;
                    let p3: i64 = n2.parse().map_err(|_| "bad year".to_string())?;
                    let (y, mo, d) = resolve_ymd3(p1, p1len, p2, p3, n2.len(), opts)?;
                    f.year = Some(y);
                    f.month = Some(mo);
                    f.day = Some(d);
                } else {
                    let (y, mo, d) = resolve_ymd2(p1, p1len, p2, n1.len(), opts)?;
                    f.year = y;
                    f.month = Some(mo);
                    f.day = d;
                }
            } else if n0.len() <= 2
                && matches!(toks.get(i + 1), Some(Tok::Sep('-')))
                && toks.get(i + 2).and_then(as_num).is_some()
            {
                // dashed M-D-Y / D-M-Y, 2 or 3 parts
                let p1: i64 = n0.parse().map_err(|_| "bad date part".to_string())?;
                let p1len = n0.len();
                i += 2;
                let n1 = match toks.get(i) {
                    Some(Tok::Num(x)) => x.clone(),
                    _ => return Err("expected date part after '-'".into()),
                };
                i += 1;
                let p2: i64 = n1.parse().map_err(|_| "bad date part".to_string())?;
                if matches!(toks.get(i), Some(Tok::Sep('-'))) {
                    i += 1;
                    let n2 = match toks.get(i) {
                        Some(Tok::Num(x)) => x.clone(),
                        _ => return Err("expected year after '-'".into()),
                    };
                    i += 1;
                    let p3: i64 = n2.parse().map_err(|_| "bad year".to_string())?;
                    let (y, mo, d) = resolve_ymd3(p1, p1len, p2, p3, n2.len(), opts)?;
                    f.year = Some(y);
                    f.month = Some(mo);
                    f.day = Some(d);
                } else {
                    let (y, mo, d) = resolve_ymd2(p1, p1len, p2, n1.len(), opts)?;
                    f.year = y;
                    f.month = Some(mo);
                    f.day = d;
                }
            } else if n0.len() <= 2
                && toks
                    .get(i + 1)
                    .and_then(as_alpha)
                    .and_then(month_from_name)
                    .is_some()
            {
                // "6 Sep 2026" / "6 Sep"
                f.day = Some(n0.parse().map_err(|_| "bad day".to_string())?);
                i += 1;
                f.month = toks
                    .get(i)
                    .and_then(as_alpha)
                    .and_then(month_from_name);
                i += 1;
                if matches!(toks.get(i), Some(Tok::Sep(','))) {
                    i += 1;
                }
                // optional year, but never swallow a time hour ("6 Sep 10:30")
                if let Some(Tok::Num(ny)) = toks.get(i) {
                    let is_time = matches!(toks.get(i + 1), Some(Tok::Sep(':')));
                    if !is_time && (ny.len() == 4 || ny.len() == 2) {
                        f.year = Some(year_from_str(ny)?);
                        i += 1;
                    }
                }
            } else if n0.len() <= 2
                && toks.get(i + 1).and_then(as_num).map(|n| n.len() <= 2).unwrap_or(false)
                && toks.get(i + 2).and_then(as_num).map(|n| n.len() <= 2).unwrap_or(false)
            {
                // spaced M D Y (all short): "10 11 12"
                let p1: i64 = n0.parse().map_err(|_| "bad date part".to_string())?;
                i += 1;
                let n1 = match toks.get(i) {
                    Some(Tok::Num(x)) => x.clone(),
                    _ => unreachable!(),
                };
                i += 1;
                let p2: i64 = n1.parse().map_err(|_| "bad date part".to_string())?;
                let n2 = match toks.get(i) {
                    Some(Tok::Num(x)) => x.clone(),
                    _ => unreachable!(),
                };
                i += 1;
                let p3: i64 = n2.parse().map_err(|_| "bad date part".to_string())?;
                let (y, mo, d) = resolve_ymd3(p1, n0.len(), p2, p3, n2.len(), opts)?;
                f.year = Some(y);
                f.month = Some(mo);
                f.day = Some(d);
            } else if n0.len() <= 2
                && toks.get(i + 1).and_then(as_num).map(|n| n.len() <= 2).unwrap_or(false)
                && (toks.get(i + 2).is_none() || matches!(toks.get(i + 2), Some(Tok::Sep(','))))
            {
                // spaced M D, no year: "9 6" / "9 6, 2026"
                let m: i64 = n0.parse().map_err(|_| "bad month".to_string())?;
                let d: i64 = toks.get(i + 1).and_then(as_num).unwrap().parse().map_err(|_| "bad day".to_string())?;
                let (mo, dy) = if opts.dayfirst { (d, m) } else { (m, d) };
                let (mo, dy) = if mo > 12 && dy <= 12 { (dy, mo) } else { (mo, dy) };
                if !(1..=12).contains(&mo) {
                    return Err(format!("month out of range: {}", mo));
                }
                f.month = Some(mo as u8);
                f.day = Some(dy as u8);
                i += 2;
                if matches!(toks.get(i), Some(Tok::Sep(','))) {
                    i += 1;
                    if let Some(Tok::Num(ny)) = toks.get(i) {
                        if ny.len() == 4 || ny.len() == 2 {
                            f.year = Some(year_from_str(ny)?);
                            i += 1;
                        }
                    }
                }
            } else {
                // lone number: >31 reads as a year, otherwise a day
                let v: i64 = n0.parse().map_err(|_| "bad number".to_string())?;
                if n0.len() == 4 || v > 31 {
                    f.year = Some(if n0.len() == 2 {
                        two_digit_year(v) as i32
                    } else {
                        v as i32
                    });
                } else {
                    f.day = Some(v as u8);
                }
                i += 1;
            }
        }
        Some(Tok::Alpha(a)) => {
            // "Sep 6 2026" / "Sep 6, 2026" / "Sep 2026" / "Sep 6"
            let mo = month_from_name(a).ok_or_else(|| format!("unknown month name '{}'", a))?;
            f.month = Some(mo);
            i += 1;
            if let Some(Tok::Num(n)) = toks.get(i) {
                let v: i64 = n.parse().map_err(|_| "bad number".to_string())?;
                if n.len() == 4 || (n.len() <= 2 && v > 31) {
                    // "Jan 99" -> 1999, "Sep 2026" -> 2026
                    f.year = Some(if n.len() == 4 {
                        v as i32
                    } else {
                        two_digit_year(v) as i32
                    });
                    i += 1;
                } else if n.len() <= 2 {
                    f.day = Some(v as u8);
                    i += 1;
                    if matches!(toks.get(i), Some(Tok::Sep(','))) {
                        i += 1;
                    }
                } else {
                    return Err(format!("unexpected number after month: {}", n));
                }
            }
            // optional year (but not a time hour like "Sep 6 10:30")
            if f.year.is_none() {
                if let Some(Tok::Num(ny)) = toks.get(i) {
                    let is_time = matches!(toks.get(i + 1), Some(Tok::Sep(':')));
                    if !is_time && (ny.len() == 4 || ny.len() == 2) {
                        f.year = Some(year_from_str(ny)?);
                        i += 1;
                    }
                }
            } else if let Some(Tok::Num(ny)) = toks.get(i) {
                // a second year-like number cannot be reconciled
                // ("Sep 40 2026" — dateutil errors too)
                let is_time = matches!(toks.get(i + 1), Some(Tok::Sep(':')));
                let ny_val: i64 = ny.parse().unwrap_or(0);
                if !is_time
                    && (ny.len() == 4 || (ny.len() <= 2 && ny_val > 31 && f.day.is_none()))
                {
                    return Err("multiple year-like values".into());
                }
            }
        }
        _ => return Err("unrecognized date layout".into()),
    }

    let has_date = f.year.is_some() || f.month.is_some() || f.day.is_some();
    let (hour, minute, second, micros, tz) = parse_time_and_offset(toks, i, has_date)?;
    f.hour = hour;
    f.minute = minute;
    f.second = second;
    f.micros = micros;
    f.tz = tz;
    Ok(f)
}

// ---------------------------------------------------------------------------
// Python glue
// ---------------------------------------------------------------------------

fn default_parts<'py>(
    py: Python<'py>,
    d: &Option<Bound<'py, PyDateTime>>,
) -> PyResult<(DefaultNum, Option<Bound<'py, PyTzInfo>>)> {
    if let Some(dt) = d {
        Ok((
            DefaultNum {
                year: dt.getattr("year")?.extract()?,
                month: dt.getattr("month")?.extract()?,
                day: dt.getattr("day")?.extract()?,
                hour: dt.getattr("hour")?.extract()?,
                minute: dt.getattr("minute")?.extract()?,
                second: dt.getattr("second")?.extract()?,
                micros: dt.getattr("microsecond")?.extract()?,
            },
            dt.getattr("tzinfo")?.extract::<Option<Bound<'py, PyTzInfo>>>()?,
        ))
    } else {
        let t = py
            .import("datetime")?
            .getattr("datetime")?
            .call_method0("today")?
            .downcast_into::<PyDateTime>()
            .map_err(PyErr::from)?;
        Ok((
            DefaultNum {
                year: t.getattr("year")?.extract()?,
                month: t.getattr("month")?.extract()?,
                day: t.getattr("day")?.extract()?,
                hour: 0,
                minute: 0,
                second: 0,
                micros: 0,
            },
            None,
        ))
    }
}

thread_local! {
    static TZ_CACHE: std::cell::RefCell<std::collections::HashMap<i32, Py<PyTzInfo>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// UTC or a cached fixed-offset timezone (datetime.timezone construction is
/// too costly to repeat per string in batch parsing).
fn fixed_tz<'py>(py: Python<'py>, offset: i32) -> PyResult<Bound<'py, PyTzInfo>> {
    if offset == 0 {
        return Ok(PyTzInfo::utc(py)?.to_owned());
    }
    if let Some(tz) = TZ_CACHE.with(|c| c.borrow().get(&offset).map(|tz| tz.clone_ref(py))) {
        return Ok(tz.into_bound(py));
    }
    let delta = PyDelta::new(py, 0, offset, 0, false)?;
    let tz = PyTzInfo::fixed_offset(py, delta)?;
    TZ_CACHE.with(|c| c.borrow_mut().insert(offset, tz.clone().unbind()));
    Ok(tz)
}

fn resolve_tz<'py>(
    py: Python<'py>,
    spec: &TzSpec,
    default_tz: Option<&Bound<'py, PyTzInfo>>,
) -> PyResult<Option<Bound<'py, PyTzInfo>>> {
    Ok(match spec {
        TzSpec::None => default_tz.cloned(),
        TzSpec::Offset(o) => Some(fixed_tz(py, *o)?),
        TzSpec::Named(name) => Some(PyTzInfo::timezone(py, name.clone()).map_err(|_| {
            ParserError::new_err(format!("rustdate: unknown timezone '{}'", name))
        })?),
    })
}

fn build_datetime<'py>(
    py: Python<'py>,
    fl: Filled,
    tz: Option<Bound<'py, PyTzInfo>>,
) -> PyResult<Bound<'py, PyDateTime>> {
    PyDateTime::new(
        py,
        fl.year,
        fl.month,
        fl.day,
        fl.hour,
        fl.minute,
        fl.second,
        fl.micros,
        tz.as_ref(),
    )
}

/// True when any component is missing and `default` must be consulted.
fn needs_default(f: &Fields) -> bool {
    f.year.is_none()
        || f.month.is_none()
        || f.day.is_none()
        || f.hour.is_none()
        || f.minute.is_none()
        || f.second.is_none()
        || f.micros.is_none()
}

fn perr(s: &str, e: String) -> PyErr {
    ParserError::new_err(format!("rustdate: could not parse {:?}: {}", s, e))
}

/// Parse a date/time string into a datetime.datetime, dateutil-style.
#[pyfunction(signature = (s, *, default = None, dayfirst = false, yearfirst = false, fuzzy = false))]
fn parse<'py>(
    py: Python<'py>,
    s: &'py str,
    default: Option<Bound<'py, PyDateTime>>,
    dayfirst: bool,
    yearfirst: bool,
    fuzzy: bool,
) -> PyResult<Bound<'py, PyDateTime>> {
    let opts = ParseOpts {
        dayfirst,
        yearfirst,
        fuzzy,
    };
    let fields = parse_inner(s, &opts).map_err(|e| perr(s, e))?;
    let (dnum, dtz) = if needs_default(&fields) {
        default_parts(py, &default)?
    } else {
        (ZEROS, None)
    };
    let filled = fill(&fields, &dnum).map_err(|e| perr(s, e))?;
    let tz = resolve_tz(py, &fields.tz, dtz.as_ref())?;
    build_datetime(py, filled, tz)
}

/// Parse a list of date/time strings in parallel (GIL released during parsing).
#[pyfunction(signature = (items, *, default = None, dayfirst = false, yearfirst = false, fuzzy = false))]
fn parse_many<'py>(
    py: Python<'py>,
    items: &Bound<'py, PyList>,
    default: Option<Bound<'py, PyDateTime>>,
    dayfirst: bool,
    yearfirst: bool,
    fuzzy: bool,
) -> PyResult<Vec<Bound<'py, PyDateTime>>> {
    let opts = ParseOpts {
        dayfirst,
        yearfirst,
        fuzzy,
    };
    let mut strings = Vec::with_capacity(items.len());
    for item in items.iter() {
        let s = item.downcast::<PyString>()?;
        strings.push(s.to_str()?.to_string());
    }
    let parsed: Vec<Result<Fields, String>> = py
        .allow_threads(|| strings.par_iter().map(|s| parse_inner(s, &opts)).collect());
    let any_missing = parsed
        .iter()
        .any(|r| r.as_ref().map(needs_default).unwrap_or(false));
    let (dnum, dtz) = if any_missing {
        default_parts(py, &default)?
    } else {
        (ZEROS, None)
    };
    let mut out = Vec::with_capacity(parsed.len());
    for (s, r) in strings.iter().zip(parsed) {
        match r {
            Ok(fields) => {
                let filled = fill(&fields, &dnum).map_err(|e| perr(s, e))?;
                let tz = resolve_tz(py, &fields.tz, dtz.as_ref())?;
                out.push(build_datetime(py, filled, tz)?);
            }
            Err(e) => return Err(perr(s, e)),
        }
    }
    Ok(out)
}

#[pymodule]
fn rustdate(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse, m)?)?;
    m.add_function(wrap_pyfunction!(parse_many, m)?)?;
    m.add("ParserError", m.py().get_type::<ParserError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MID: DefaultNum = DefaultNum {
        year: 2020,
        month: 3,
        day: 15,
        hour: 0,
        minute: 0,
        second: 0,
        micros: 0,
    };
    const DEF: DefaultNum = DefaultNum {
        year: 2020,
        month: 3,
        day: 15,
        hour: 7,
        minute: 8,
        second: 9,
        micros: 123000,
    };
    const STRICT: ParseOpts = ParseOpts {
        dayfirst: false,
        yearfirst: false,
        fuzzy: false,
    };

    fn pf(s: &str, opts: &ParseOpts) -> Fields {
        parse_inner(s, opts).unwrap_or_else(|e| panic!("{}: {}", s, e))
    }

    fn fl(s: &str, opts: &ParseOpts, d: &DefaultNum) -> Filled {
        let f = pf(s, opts);
        fill(&f, d).unwrap_or_else(|e| panic!("{}: {}", s, e))
    }

    fn p(s: &str) -> Filled {
        fl(s, &STRICT, &MID)
    }

    #[test]
    fn iso() {
        let f = pf("2026-09-06T14:23:45Z", &STRICT);
        assert_eq!(
            (f.year, f.month, f.day, f.hour, f.minute, f.second),
            (Some(2026), Some(9), Some(6), Some(14), Some(23), Some(45))
        );
        assert_eq!(f.tz, TzSpec::Offset(0));
        let f = pf("2026-09-06 14:23:45+05:30", &STRICT);
        assert_eq!(f.tz, TzSpec::Offset(19800));
        let f = pf("2026-09-06T14:23:45.123456-07:00", &STRICT);
        assert_eq!(f.micros, Some(123456));
        assert_eq!(f.tz, TzSpec::Offset(-25200));
        let f = pf("2026-09-06T14:23", &STRICT);
        assert_eq!((f.hour, f.minute, f.second), (Some(14), Some(23), None));
        assert_eq!(f.tz, TzSpec::None);
        let x = p("2026-09-06T14:23:45+0530");
        assert_eq!(x.hour, 14);
    }

    #[test]
    fn human() {
        let f = pf("Sep 6 2026 2:23 PM", &STRICT);
        assert_eq!(
            (f.month, f.day, f.year, f.hour, f.minute),
            (Some(9), Some(6), Some(2026), Some(14), Some(23))
        );
        let f = pf("6 Sep 2026", &STRICT);
        assert_eq!((f.month, f.day, f.year, f.hour), (Some(9), Some(6), Some(2026), None));
        let f = pf("Sun, 06 Sep 2026 14:23:45 GMT", &STRICT);
        assert_eq!(f.tz, TzSpec::Offset(0));
        // p.m. spelling and jump words
        let x = p("2:23 p.m.");
        assert_eq!(x.hour, 14);
        let x = p("Sep 6 2026 at 2 PM");
        assert_eq!(x.hour, 14);
    }

    #[test]
    fn slash() {
        let x = p("06/09/2026 14:23:45");
        assert_eq!((x.month, x.day, x.year, x.hour), (6, 9, 2026, 14));
        let x = p("12/25/26 8:05 AM");
        assert_eq!((x.month, x.day, x.year, x.hour), (12, 25, 2026, 8));
        // year-first slash and two-part dates
        let x = p("2026/09/06");
        assert_eq!((x.year, x.month, x.day), (2026, 9, 6));
        let x = p("9/6");
        assert_eq!((x.month, x.day), (9, 6));
        // dateutil's >12 correction
        let x = p("25/12/2026");
        assert_eq!((x.month, x.day), (12, 25));
    }

    #[test]
    fn dayfirst_and_dashed() {
        let opts = ParseOpts {
            dayfirst: true,
            ..STRICT
        };
        let x = fl("06/09/2026", &opts, &MID);
        assert_eq!((x.month, x.day, x.year), (9, 6, 2026));
        let x = p("13/05/2026");
        assert_eq!((x.month, x.day), (5, 13));
        let x = p("12-25-2026 8:05");
        assert_eq!((x.month, x.day, x.year, x.hour), (12, 25, 2026, 8));
        let x = fl("25-12-2026", &opts, &MID);
        assert_eq!((x.month, x.day), (12, 25));
    }

    #[test]
    fn yearfirst() {
        let x = p("10-11-12");
        assert_eq!((x.year, x.month, x.day), (2012, 10, 11));
        let opts = ParseOpts {
            yearfirst: true,
            ..STRICT
        };
        let x = fl("10-11-12", &opts, &MID);
        assert_eq!((x.year, x.month, x.day), (2010, 11, 12));
    }

    #[test]
    fn week_ordinal_compact() {
        let x = p("2026-W36-6");
        assert_eq!((x.year, x.month, x.day), (2026, 9, 5));
        let x = p("2026-249");
        assert_eq!((x.year, x.month, x.day), (2026, 9, 6));
        let x = p("20260906T143022Z");
        assert_eq!(
            (x.year, x.month, x.day, x.hour, x.minute, x.second),
            (2026, 9, 6, 14, 30, 22)
        );
        let x = p("2026 09 06");
        assert_eq!((x.year, x.month, x.day), (2026, 9, 6));
        let x = p("2024-060");
        assert_eq!((x.year, x.month, x.day), (2024, 2, 29)); // leap year
    }

    #[test]
    fn time_only_and_defaults() {
        let x = fl("14:23:45", &STRICT, &DEF);
        assert_eq!(
            (x.year, x.month, x.day, x.hour, x.minute, x.second),
            (2020, 3, 15, 14, 23, 45)
        );
        // missing pieces copied from default, exactly like dateutil
        let x = fl("Sep 6", &STRICT, &DEF);
        assert_eq!((x.year, x.month, x.day, x.hour, x.minute, x.second), (2020, 9, 6, 7, 8, 9));
        let x = fl("2026", &STRICT, &DEF);
        assert_eq!((x.year, x.month, x.day), (2026, 3, 15));
        let x = fl("2026-09", &STRICT, &DEF);
        assert_eq!((x.year, x.month, x.day), (2026, 9, 15));
        let x = fl("Sep 2026", &STRICT, &MID);
        assert_eq!((x.year, x.month, x.day), (2026, 9, 15));
    }

    #[test]
    fn fuzzy() {
        let opts = ParseOpts {
            fuzzy: true,
            ..STRICT
        };
        let x = fl("I met him on Sep 6 2026 at 2 PM", &opts, &MID);
        assert_eq!(
            (x.year, x.month, x.day, x.hour),
            (2026, 9, 6, 14)
        );
        let x = fl("meeting Thursday 2026-09-06 10:30 ok", &opts, &MID);
        assert_eq!((x.year, x.month, x.day, x.hour, x.minute), (2026, 9, 6, 10, 30));
        // strict mode must reject the same strings
        assert!(parse_inner("I met him on Sep 6 2026 at 2 PM", &STRICT).is_err());
    }

    #[test]
    fn tz_specs() {
        // dateutil default: abbreviation recognized but dropped → naive
        let f = pf("2026-09-06 14:23:45 IST", &STRICT);
        assert_eq!(f.tz, TzSpec::None);
        let f = pf("2026-09-06 14:23:45 UTC", &STRICT);
        assert_eq!(f.tz, TzSpec::Offset(0));
        let f = pf("2026-09-06 EST5EDT", &STRICT);
        assert_eq!(f.tz, TzSpec::Named("EST5EDT".into()));
        let f = pf("2026-09-06 Asia/Kolkata", &STRICT);
        assert_eq!(f.tz, TzSpec::Named("Asia/Kolkata".into()));
    }

    #[test]
    fn dateutil_quirks() {
        // dateutil applies dayfirst even to ISO dates
        let opts = ParseOpts {
            dayfirst: true,
            ..STRICT
        };
        let x = fl("2026-09-06", &opts, &MID);
        assert_eq!((x.month, x.day), (6, 9));
        let x = fl("2026/06/09", &opts, &MID);
        assert_eq!((x.month, x.day), (9, 6));
        // yearfirst backtracking: month 22 is impossible
        let yf = ParseOpts {
            yearfirst: true,
            ..STRICT
        };
        let x = fl("01-22-15", &yf, &MID);
        assert_eq!((x.year, x.month, x.day), (2015, 1, 22));
        // 2-digit value > 31 after month name reads as year
        let x = fl("Jan 99", &STRICT, &MID);
        assert_eq!((x.year, x.month, x.day), (1999, 1, 15));
        // AM/PM edge cases
        let x = fl("00:29 PM", &STRICT, &MID);
        assert_eq!(x.hour, 12);
        let x = fl("12:40 AM", &STRICT, &MID);
        assert_eq!(x.hour, 0);
        assert!(parse_inner("15:09 AM", &STRICT).is_err());
    }

    #[test]
    fn rejects() {
        for bad in ["", "garbage", "25/13/2026", "2026-13-40", "2026-366", "Sep 40 2026"] {
            assert!(parse_inner(bad, &STRICT).is_err(), "should reject {:?}", bad);
        }
        // day-level validation happens at fill time
        for bad in ["2026-02-30", "Feb 30 2026"] {
            assert!(fill(&pf(bad, &STRICT), &MID).is_err(), "fill should reject {:?}", bad);
        }
    }
}
