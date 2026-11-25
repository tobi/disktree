//! Human-readable size, age, and bar formatting helpers.

/// Format a byte count with binary IEC prefixes (`B`, `KiB`, `MiB`, `GiB`,
/// `TiB`).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Format an elapsed duration in seconds as a compact relative age string.
pub fn human_age(age_secs: u64) -> String {
    if age_secs < 5 {
        "just now".to_owned()
    } else if age_secs < 60 {
        format!("{age_secs}s ago")
    } else if age_secs < 3_600 {
        format!("{}m ago", age_secs / 60)
    } else if age_secs < 86_400 {
        format!("{}h ago", age_secs / 3_600)
    } else {
        format!("{}d ago", age_secs / 86_400)
    }
}

/// Render an ASCII/Unicode usage bar of width `width` for `fraction` in
/// `0.0..=1.0`.
pub fn usage_bar(fraction: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut out = String::with_capacity(width * 3);
    for _ in 0..filled {
        out.push('█');
    }
    for _ in filled..width {
        out.push('░');
    }
    out
}
