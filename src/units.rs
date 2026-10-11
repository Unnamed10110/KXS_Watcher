//! Values as people read them: durations and ages, quantities (CPU, memory), and cell ordering/matching.

use std::cmp::Ordering;

/// kubectl's HumanDuration.
pub fn human(secs: i64) -> String {
    let s = secs.max(0);
    let (m, h) = (s / 60, s / 3600);
    let d = h / 24;
    match () {
        _ if s < 120 => format!("{s}s"),
        _ if m < 10 && s % 60 == 0 => format!("{m}m"),
        _ if m < 10 => format!("{m}m{}s", s % 60),
        _ if m < 180 => format!("{m}m"),
        _ if h < 8 && m % 60 == 0 => format!("{h}h"),
        _ if h < 8 => format!("{h}h{}m", m % 60),
        _ if h < 48 => format!("{h}h"),
        _ if h < 192 && h % 24 == 0 => format!("{d}d"),
        _ if h < 192 => format!("{d}d{}h", h % 24),
        _ if d < 730 => format!("{d}d"),
        _ if d < 2920 && d % 365 == 0 => format!("{}y", d / 365),
        _ if d < 2920 => format!("{}y{}d", d / 365, d % 365),
        _ => format!("{}y", d / 365),
    }
}

pub fn age(ts: Option<jiff::Timestamp>) -> String {
    ts.map(|t| human(jiff::Timestamp::now().as_second() - t.as_second())).unwrap_or_default()
}

/// Age for a `date` typed cell (RFC3339 string); passes other text through.
pub fn date_cell(text: &str) -> String {
    text.parse::<jiff::Timestamp>().map(|t| age(Some(t))).unwrap_or_else(|_| text.to_string())
}

fn lead_num(s: &str) -> Option<f64> {
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// Numeric when both cells start with a number (`3`, `1/2`, `5 (3m ago)`), else case-insensitive text.
// ponytail: ignores units (128Mi vs 1Gi); parse quantities if someone sorts by memory columns.
pub fn cmp_cells(a: &str, b: &str) -> Ordering {
    match (lead_num(a), lead_num(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal).then_with(|| a.cmp(b)),
        _ => a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase())),
    }
}

/// Kubernetes quantity → f64 (cores for CPU, bytes for memory).
pub fn parse_qty(s: &str) -> f64 {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+')).unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let n: f64 = num.parse().unwrap_or(0.0);
    let mult = match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        e if e.starts_with(['e', 'E']) => 10f64.powf(e[1..].parse().unwrap_or(0.0)),
        _ => 1.0,
    };
    n * mult
}

pub fn fmt_cpu(cores: f64) -> String {
    if cores < 1.0 { format!("{:.0}m", cores * 1000.0) } else { format!("{cores:.2}") }
}

pub fn fmt_bytes(b: f64) -> String {
    const U: [&str; 5] = ["B", "Ki", "Mi", "Gi", "Ti"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i >= 3 { format!("{v:.1}{}", U[i]) } else { format!("{v:.0}{}", U[i]) }
}
