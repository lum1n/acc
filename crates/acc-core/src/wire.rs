//! Small helpers shared by the JSON-over-stdio drivers.

use serde_json::Value;

/// Upper bound for one NDJSON frame. Claude's `initialize` reply and Codex
/// thread snapshots can be large, but nothing legitimate comes close.
pub(crate) const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Serializes one NDJSON record, newline included.
pub(crate) fn line(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(v).unwrap_or_default();
    out.push(b'\n');
    out
}

pub(crate) fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn owned(v: &Value, key: &str) -> Option<String> {
    str_at(v, key).map(str::to_owned)
}

pub(crate) fn u64_at(v: &Value, key: &str) -> Option<u64> {
    let x = v.get(key)?;
    x.as_u64()
        .or_else(|| x.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

pub(crate) fn f64_at(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// Sums the given integer fields, treating missing ones as zero. `None` if all are missing.
pub(crate) fn sum(v: &Value, keys: &[&str]) -> Option<u64> {
    let mut any = false;
    let mut total = 0u64;
    for k in keys {
        if let Some(n) = u64_at(v, k) {
            any = true;
            total = total.saturating_add(n);
        }
    }
    any.then_some(total)
}

/// First line of `text`, trimmed and capped, for titles and state messages.
pub(crate) fn one_line(text: &str, max_chars: usize) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if first.chars().count() <= max_chars {
        return first.to_owned();
    }
    let mut out: String = first.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sum_and_numbers() {
        let v = json!({"a": 1, "b": 2.0, "c": "x"});
        assert_eq!(sum(&v, &["a", "b", "z"]), Some(3));
        assert_eq!(sum(&v, &["z"]), None);
        assert_eq!(u64_at(&v, "c"), None);
    }

    #[test]
    fn one_line_caps() {
        assert_eq!(one_line("\n  hello world\nsecond", 5), "hell…");
        assert_eq!(one_line("ok", 5), "ok");
    }
}
