//! Human-readable sizes and counts.
//!
//! Binary units, like `du -h` and dust. One decimal below 10, none above 100:
//! enough resolution to compare neighbours without a wall of digits.

const BYTE_UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/// `1.4 GiB`, `523 MiB`, `12 B`.
pub fn human_bytes(bytes: u64) -> String {
    scale(bytes).format(1)
}

/// `1.4G`, `523M`, `12B`: for tile labels, where columns are scarce.
pub fn human_bytes_short(bytes: u64) -> String {
    scale(bytes).format(0)
}

fn scale(bytes: u64) -> Scaled {
    let mut value = bytes as f64;
    let mut unit = 0;
    // Step up from 1023.5, not 1024: no decimal is shown at that size, so
    // anything above it would print as `1024 KiB` instead of `1.0 MiB`.
    while value >= 1023.5 && unit + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    Scaled { value, unit }
}

struct Scaled {
    value: f64,
    unit: usize,
}

impl Scaled {
    fn format(&self, gap: usize) -> String {
        // Whole bytes have no fraction to show: `0 B`, not `0.0 B`.
        let text = if self.unit == 0 {
            format!("{:.0}", self.value)
        } else if self.value < 9.95 {
            format!("{:.1}", self.value)
        } else {
            format!("{:.0}", self.value)
        };
        if gap == 0 {
            format!("{text}{}", BYTE_UNITS[self.unit])
        } else {
            format!("{text} {}", BYTE_UNITS[self.unit])
        }
    }
}

/// `1.2k`, `3.4M`, `812`: file counts, which grow past a million quickly.
pub fn human_count(count: u64) -> String {
    let value = count as f64;
    // Each bound sits where the one decimal would round up to 1000.0, so
    // 999,999 reads `1.0M`, not `1000.0k`.
    if count < 10_000 {
        format!("{count}")
    } else if value < 999_950.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value < 999_950_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else {
        format!("{:.1}G", value / 1_000_000_000.0)
    }
}

/// A size as typed on a command line: `512M`, `2G`, `1.5GiB`, `800k`.
///
/// A plain number is bytes. Units are binary however they are spelled,
/// because every size here is printed in them: `1G` is what `1.0 GiB` means.
pub fn parse_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|char: char| !(char.is_ascii_digit() || char == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let power = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 1,
        "m" | "mb" | "mib" => 2,
        "g" | "gb" | "gib" => 3,
        "t" | "tb" | "tib" => 4,
        "p" | "pb" | "pib" => 5,
        _ => return None,
    };
    let bytes = number * 1024_f64.powi(power);
    (bytes < u64::MAX as f64).then(|| bytes.round() as u64)
}

/// Share of `total` as a whole percentage, saturating instead of dividing by
/// zero.
pub fn share(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64) * 100.0
    }
}

/// Compass width for a share, e.g. `▓▓▓░░`.
pub fn share_bar(part: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((part as f64 / total as f64) * width as f64).round() as usize
    };
    let filled = filled.min(width);
    let mut bar = String::with_capacity(width);
    for index in 0..width {
        bar.push(if index < filled { '▓' } else { '░' });
    }
    bar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_binary_units_with_one_decimal_below_ten() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(7), "7 B");
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(
            human_bytes(1024 * 1024 * 1024 + 400 * 1024 * 1024),
            "1.4 GiB"
        );
        assert_eq!(human_bytes(512 * 1024 * 1024), "512 MiB");
    }

    #[test]
    fn short_form_drops_the_space() {
        assert_eq!(human_bytes_short(1536), "1.5KiB");
        assert_eq!(human_bytes_short(512 * 1024), "512KiB");
    }

    #[test]
    fn counts_switch_to_metric_suffixes() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(9_999), "9999");
        assert_eq!(human_count(12_000), "12.0k");
        assert_eq!(human_count(2_500_000), "2.5M");
    }

    #[test]
    fn a_value_that_rounds_up_to_the_next_unit_is_shown_in_it() {
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024 * 1024 - 1), "1.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024 - 1), "1.0 GiB");
        assert_eq!(human_bytes_short(1024 * 1024 - 1), "1.0MiB");
        assert_eq!(human_bytes(1023 * 1024), "1023 KiB");
        assert_eq!(human_count(999_949), "999.9k");
        assert_eq!(human_count(999_999), "1.0M");
        assert_eq!(human_count(999_999_999), "1.0G");
    }

    #[test]
    fn typed_sizes_are_binary_whatever_the_spelling() {
        assert_eq!(parse_bytes("4096"), Some(4096));
        assert_eq!(parse_bytes("1k"), Some(1024));
        assert_eq!(parse_bytes("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_bytes("512 MiB"), parse_bytes("512mb"));
        assert_eq!(parse_bytes("1.5G"), Some(3 * 512 * 1024 * 1024));
        assert_eq!(parse_bytes("2TB"), Some(2 << 40));
        for bad in ["", "G", "1.2.3G", "-1G", "1 lots", "1e3", "99999999P"] {
            assert_eq!(parse_bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn share_survives_a_zero_total() {
        assert!((share(5, 0) - 0.0).abs() < f64::EPSILON);
        assert!((share(1, 4) - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn share_bar_is_always_the_requested_width() {
        assert_eq!(share_bar(1, 4, 4).chars().count(), 4);
        assert_eq!(share_bar(4, 4, 4), "▓▓▓▓");
        assert_eq!(share_bar(0, 4, 4), "░░░░");
        assert_eq!(share_bar(1, 0, 3).chars().count(), 3);
    }
}
