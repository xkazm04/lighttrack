//! Markdown primitives shared by every renderer: an aligned GFM table builder, a unicode sparkline,
//! status glyphs, and compact number / time / id formatting. All pure string work.
//!
//! Note: column padding uses `chars().count()`, so cells containing wide glyphs (✅, ⚠️) can be
//! a column off in the *raw* text view — Markdown renderers re-align it, and we avoid a
//! unicode-width dependency to keep this crate zero-dep beyond `serde_json`.

use serde_json::Value;

/// Column alignment.
#[derive(Clone, Copy)]
pub(crate) enum Align {
    Left,
    Right,
}

/// A small GitHub-flavored-markdown table builder. Cells are padded to the column width so the raw
/// text (the MCP tool-output panel, or a piped CLI run) stays readable; Markdown renderers collapse
/// the padding away.
pub(crate) struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub(crate) fn new(cols: &[(&str, Align)]) -> Self {
        Table {
            headers: cols.iter().map(|(h, _)| (*h).to_string()).collect(),
            aligns: cols.iter().map(|(_, a)| *a).collect(),
            rows: Vec::new(),
        }
    }

    /// Add a row. Every cell is made safe for one table cell on the way in: a `|` inside it would
    /// start a new column and a newline a new row, so a dataset input of `2|3?` or a multi-line
    /// alert message used to break the table for every row after it. Escaping here, once, covers
    /// every renderer rather than trusting each of forty call sites to remember.
    pub(crate) fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells.iter().map(|c| cell(c)).collect());
    }

    pub(crate) fn render(&self) -> String {
        let n = self.headers.len();
        let mut width: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                width[i] = width[i].max(c.chars().count());
            }
        }
        let mut out = String::new();
        out.push_str(&render_row(&self.headers, &width, &self.aligns));
        out.push_str(&separator(&width, &self.aligns));
        for r in &self.rows {
            out.push_str(&render_row(r, &width, &self.aligns));
        }
        out
    }
}

/// One string as one GFM table cell: pipes escaped, line breaks collapsed to a space.
pub(crate) fn cell(s: &str) -> String {
    if !s.contains(['|', '\n', '\r']) {
        return s.to_string();
    }
    s.replace("\r\n", " ")
        .replace(['\n', '\r'], " ")
        .replace('|', "\\|")
}

fn render_row(cells: &[String], width: &[usize], aligns: &[Align]) -> String {
    let mut s = String::from("|");
    for i in 0..width.len() {
        let c = cells.get(i).map(String::as_str).unwrap_or("");
        s.push(' ');
        s.push_str(&pad(c, width[i], aligns[i]));
        s.push_str(" |");
    }
    s.push('\n');
    s
}

fn separator(width: &[usize], aligns: &[Align]) -> String {
    let mut s = String::from("|");
    for i in 0..width.len() {
        match aligns[i] {
            Align::Left => s.push_str(&"-".repeat(width[i] + 2)),
            // trailing colon = right-aligned in GFM
            Align::Right => {
                s.push_str(&"-".repeat(width[i] + 1));
                s.push(':');
            }
        }
        s.push('|');
    }
    s.push('\n');
    s
}

fn pad(s: &str, w: usize, a: Align) -> String {
    let len = s.chars().count();
    if len >= w {
        return s.to_string();
    }
    let fill = " ".repeat(w - len);
    match a {
        Align::Left => format!("{s}{fill}"),
        Align::Right => format!("{fill}{s}"),
    }
}

/// An 8-level unicode sparkline over `xs`, min→max normalized. Flat input renders mid-height.
pub(crate) fn sparkline(xs: &[f64]) -> String {
    const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if xs.is_empty() {
        return String::new();
    }
    let mn = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let mx = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = mx - mn;
    xs.iter()
        .map(|&x| {
            let idx = if span <= f64::EPSILON {
                3
            } else {
                (((x - mn) / span) * 7.0).round() as usize
            };
            TICKS[idx.min(7)]
        })
        .collect()
}

/// Glyph for an optional pass/fail flag.
pub(crate) fn pass_glyph(p: Option<bool>) -> &'static str {
    match p {
        Some(true) => "✅",
        Some(false) => "❌",
        None => "·",
    }
}

/// Glyph for a free-form status string (run / job / event states).
pub(crate) fn status_glyph(s: &str) -> &'static str {
    match s {
        "passed" | "done" | "success" | "completed" | "compared" => "✅",
        "regressed" | "failed" | "error" | "timeout" => "❌",
        "running" | "claimed" => "⏳",
        "queued" => "•",
        _ => "·",
    }
}

/// Money with magnitude-aware precision: tiny costs keep 5 dp so sub-cent judge spend stays visible.
/// Negatives read as `-$1.23` (sign before the symbol), which margins rely on.
pub(crate) fn money(x: f64) -> String {
    let sign = if x < 0.0 { "-" } else { "" };
    let a = x.abs();
    if a == 0.0 {
        "$0".to_string()
    } else if a < 0.01 {
        format!("{sign}${a:.5}")
    } else {
        format!("{sign}${a:.2}")
    }
}

/// A per-million-token price-book rate (sub-dollar rates need 3 dp).
pub(crate) fn rate(x: f64) -> String {
    if x < 1.0 {
        format!("${x:.3}")
    } else {
        format!("${x:.2}")
    }
}

/// Group an integer with thousands separators: `1234567` → `1,234,567`.
pub(crate) fn commafy(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A 0–1 fraction as a whole-number percent.
pub(crate) fn pct(frac: f64) -> String {
    format!("{:.0}%", frac * 100.0)
}

/// Trim an RFC3339 timestamp to `MM-DD HH:MM` for compact tables. Anything not shaped like
/// `YYYY-MM-DDTHH:MM` is returned unchanged: the old check (`len >= 16`) sliced any long string at
/// byte offsets 5, 10 and 11, which turned a sentence into fragments and **panicked** on a
/// multi-byte character straddling one of those offsets — inside a renderer, that is a crashed
/// MCP tool call for a stored value that was merely not a timestamp.
pub(crate) fn short_ts(s: &str) -> String {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let shaped = b.len() >= 16
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && (b[10] == b'T' || b[10] == b' ')
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16);
    if shaped {
        format!("{} {}", &s[5..10], &s[11..16])
    } else {
        s.to_string()
    }
}

/// Truncate to at most `n` chars, appending `…` when shortened.
pub(crate) fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

// --- small Value accessors so renderers stay terse -------------------------------------------------

pub(crate) fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}
pub(crate) fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}
pub(crate) fn opt_f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}
pub(crate) fn opt_u(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}
pub(crate) fn opt_b(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}
pub(crate) fn opt_s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_header_separator_and_rows() {
        let mut t = Table::new(&[("A", Align::Left), ("N", Align::Right)]);
        t.row(vec!["x".into(), "10".into()]);
        let out = t.render();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("| A"));
        assert!(lines[1].contains(':')); // right-align marker on column N
        assert!(lines[2].contains("10"));
    }

    /// A `|` or a newline in a cell used to corrupt the table from that row on.
    #[test]
    fn a_cell_cannot_break_out_of_its_column_or_row() {
        let mut t = Table::new(&[("A", Align::Left), ("B", Align::Left)]);
        t.row(vec!["what is 2|3?".into(), "line one\nline two".into()]);
        let out = t.render();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "one header, one separator, ONE row: {out}");
        assert_eq!(lines[2].matches('|').count(), 3 + 1, "{out}");
        assert!(lines[2].contains("2\\|3?") && lines[2].contains("line one line two"));
        assert_eq!(cell("plain"), "plain");
    }

    #[test]
    fn sparkline_spans_levels() {
        let s = sparkline(&[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(s.chars().count(), 4);
        assert!(s.starts_with('▁'));
        assert!(s.ends_with('█'));
        assert!(sparkline(&[]).is_empty());
    }

    #[test]
    fn commafy_groups_thousands() {
        assert_eq!(commafy(0), "0");
        assert_eq!(commafy(999), "999");
        assert_eq!(commafy(1234567), "1,234,567");
    }

    #[test]
    fn short_ts_trims_rfc3339() {
        assert_eq!(short_ts("2026-06-17T12:34:56.789Z"), "06-17 12:34");
        assert_eq!(short_ts("2026-06-17 12:34:56"), "06-17 12:34");
        assert_eq!(short_ts("short"), "short");
    }

    /// A long string that is not a timestamp is returned whole, and a multi-byte one no longer
    /// panics at a byte offset inside a character.
    #[test]
    fn short_ts_leaves_non_timestamps_alone() {
        assert_eq!(short_ts("not a timestamp at all"), "not a timestamp at all");
        let accented = "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}";
        assert_eq!(short_ts(accented), accented);
        assert_eq!(short_ts("2026-06-17Tnot:ok"), "2026-06-17Tnot:ok");
    }

    #[test]
    fn trunc_appends_ellipsis() {
        assert_eq!(trunc("hello", 10), "hello");
        assert_eq!(trunc("hello world", 6), "hello…");
    }
}
