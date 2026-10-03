//! Human-friendly sizes and durations for the CLI: `50G`, `500M`, `36h`, `7d`.

use anyhow::{Result, bail};

pub fn parse_size(text: &str) -> Result<i64> {
    let (number, unit) = split(text);
    let factor: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024f64.powi(2),
        "g" | "gb" | "gib" => 1024f64.powi(3),
        "t" | "tb" | "tib" => 1024f64.powi(4),
        _ => bail!("cannot read size {text:?}; try 500M, 50G or 2T"),
    };
    positive(number, factor, text)
}

pub fn parse_duration_secs(text: &str) -> Result<i64> {
    let (number, unit) = split(text);
    let factor: f64 = match unit {
        "" | "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        "w" => 7.0 * 86400.0,
        _ => bail!("cannot read duration {text:?}; try 90m, 36h or 7d"),
    };
    positive(number, factor, text)
}

pub fn format_size(bytes: i64) -> String {
    let mut value = bytes as f64;
    for unit in ["B", "K", "M", "G"] {
        if value < 1024.0 {
            return format!("{value:.1}{unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1}T")
}

pub fn format_duration(secs: i64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h:02}h"),
    }
}

fn split(text: &str) -> (&str, &str) {
    let text = text.trim();
    let at = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    (&text[..at], text[at..].trim())
}

fn positive(number: &str, factor: f64, text: &str) -> Result<i64> {
    let value: f64 = number.parse().map_err(|_| anyhow::anyhow!("cannot read {text:?}"))?;
    if value <= 0.0 {
        bail!("{text:?} must be positive");
    }
    Ok((value * factor).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("50G").unwrap(), 50 << 30);
        assert_eq!(parse_size("1.5 GB").unwrap(), 3 << 29);
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert!(parse_size("5X").is_err());
        assert!(parse_size("0").is_err());
        assert_eq!(format_size(3 << 29), "1.5G");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("36h").unwrap(), 36 * 3600);
        assert_eq!(parse_duration_secs("7d").unwrap(), 7 * 86400);
        assert_eq!(parse_duration_secs("90").unwrap(), 90);
        assert!(parse_duration_secs("h").is_err());
        assert_eq!(format_duration(90 * 60), "1h30m");
        assert_eq!(format_duration(26 * 3600), "1d02h");
    }
}
