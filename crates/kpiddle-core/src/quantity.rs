//! Kubernetes resource quantities as numbers.
//!
//! The API serializes quantities as strings: `250m` CPU, `128Mi` memory,
//! metrics-server's `123456789n` nanocores. Binary suffixes (`Ki` … `Ei`),
//! decimal suffixes (`n` … `E`) and exponents (`1e3`) are accepted.

/// CPU in millicores: `"250m"` → 250, `"2"` → 2000, `"1500000n"` → 1.5.
pub fn cpu_millis(q: &str) -> Option<f64> {
    parse(q).map(|v| v * 1000.0)
}

/// Memory in bytes: `"128Mi"` → 134217728, `"1G"` → 1000000000.
pub fn bytes(q: &str) -> Option<u64> {
    parse(q).map(|v| v.round() as u64)
}

/// A quantity in base units, or `None` if it is malformed or negative.
fn parse(q: &str) -> Option<f64> {
    let q = q.trim();
    let split = q.find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '+' | '-'))).unwrap_or(q.len());
    let (num, suffix) = q.split_at(split);
    let n: f64 = num.parse().ok()?;
    const KI: f64 = 1024.0;
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
        "Ki" => KI,
        "Mi" => KI.powi(2),
        "Gi" => KI.powi(3),
        "Ti" => KI.powi(4),
        "Pi" => KI.powi(5),
        "Ei" => KI.powi(6),
        s if s.starts_with(['e', 'E']) => 10f64.powi(s[1..].parse().ok()?),
        _ => return None,
    };
    let v = n * mult;
    (v.is_finite() && v >= 0.0).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu() {
        assert_eq!(cpu_millis("250m"), Some(250.0));
        assert_eq!(cpu_millis("2"), Some(2000.0));
        assert_eq!(cpu_millis("0.5"), Some(500.0));
        assert!((cpu_millis("1500000n").unwrap() - 1.5).abs() < 1e-9);
        assert_eq!(cpu_millis("0"), Some(0.0));
    }

    #[test]
    fn memory() {
        assert_eq!(bytes("128Mi"), Some(128 << 20));
        assert_eq!(bytes("1G"), Some(1_000_000_000));
        assert_eq!(bytes("123456Ki"), Some(123456 << 10));
        assert_eq!(bytes("1e3"), Some(1000));
        assert_eq!(bytes("2E"), Some(2_000_000_000_000_000_000));
    }

    #[test]
    fn malformed() {
        for q in ["", "m", "12x", "1e", "-1", "1.2.3", "Mi"] {
            assert_eq!(bytes(q), None, "{q:?}");
        }
    }
}
