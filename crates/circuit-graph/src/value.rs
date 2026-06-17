//! Engineering-notation value parsing (`"22pF"`, `"4k7"`, `"8MHz"`, `"100n"`).
//!
//! Idiom node predicates can constrain a part's *value* (a crystal-load cap is
//! tens of pF; a bulk cap is µF) — but values arrive as free-text strings with SI
//! prefixes, unit suffixes, and the RKM "decimal-in-the-prefix" convention. This
//! folds them all to a single number in base SI units (farads, ohms, hertz, …),
//! so a predicate can say "between 1pF and 1nF" once and have it just work.

/// Parse an engineering-notation value to base SI units, or `None` if it is not a
/// recognizable number. Unit-agnostic: `"22pF"`, `"22p"`, `"22pf"` all give
/// `22e-12`. Handles the RKM convention where the prefix doubles as the decimal
/// point (`"4k7"` = 4700, `"R47"` = 0.47, `"2M2"` = 2.2e6).
pub fn parse_eng(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let prefix_of = |c: char| -> Option<f64> {
        match c {
            'p' => Some(1e-12),
            'n' => Some(1e-9),
            'u' | 'µ' => Some(1e-6),
            'm' => Some(1e-3),
            'k' | 'K' => Some(1e3),
            'M' => Some(1e6),
            'G' => Some(1e9),
            'R' | 'r' => Some(1.0), // RKM: bare resistance, prefix = ones place
            _ => None,
        }
    };

    let chars: Vec<char> = s.chars().collect();
    // Leading numeric run (digits and a decimal point).
    let split = chars.iter().position(|c| !(c.is_ascii_digit() || *c == '.')).unwrap_or(chars.len());
    let head: String = chars[..split].iter().collect();
    let rest: String = chars[split..].iter().collect();

    // Pure number, no suffix.
    if rest.is_empty() {
        return head.parse().ok();
    }

    let first = rest.chars().next().unwrap();
    let Some(mult) = prefix_of(first) else {
        // First non-digit is a plain unit, not a prefix ("5V", "470Ω"): the number
        // is the head as-is. If the head is empty, this is not a value at all.
        return head.parse().ok();
    };
    let tail: String = rest.chars().skip(1).collect();

    // RKM / embedded-prefix form: prefix sits between digits ("4k7", "R47").
    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
        let int: f64 = if head.is_empty() { 0.0 } else { head.parse().ok()? };
        let frac: f64 = tail.parse::<f64>().ok()? / 10f64.powi(tail.chars().count() as i32);
        return Some((int + frac) * mult);
    }

    // Plain "<number><prefix><unit>" ("22pF", "10k", "8MHz"): scale the head.
    let v: f64 = head.parse().ok()?;
    Some(v * mult)
}

#[cfg(test)]
mod tests {
    use super::parse_eng;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() <= b.abs() * 1e-9 + 1e-15, "{a} != {b}");
    }

    #[test]
    fn si_suffixes() {
        approx(parse_eng("22pF").unwrap(), 22e-12);
        approx(parse_eng("22p").unwrap(), 22e-12);
        approx(parse_eng("100nF").unwrap(), 100e-9);
        approx(parse_eng("4.7uF").unwrap(), 4.7e-6);
        approx(parse_eng("10k").unwrap(), 10e3);
        approx(parse_eng("8MHz").unwrap(), 8e6);
        approx(parse_eng("470").unwrap(), 470.0);
    }

    #[test]
    fn rkm_notation() {
        approx(parse_eng("4k7").unwrap(), 4700.0);
        approx(parse_eng("2M2").unwrap(), 2.2e6);
        approx(parse_eng("R47").unwrap(), 0.47);
        approx(parse_eng("1R0").unwrap(), 1.0);
    }

    #[test]
    fn junk_is_none() {
        assert!(parse_eng("").is_none());
        assert!(parse_eng("DNP").is_none());
        // A part *name* with non-numeric head is not a value (the "ST" before the
        // 'M' prefix won't parse), so it folds to None rather than a bogus number.
        assert!(parse_eng("STM32F103").is_none());
    }
}
