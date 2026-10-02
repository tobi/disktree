//! Numbers the way GNU reads and writes them: gnulib's `xstrtol` for
//! option arguments and `human_readable` for sizes. Both are ported rule for
//! rule, because du's output is compared byte for byte and the rounding in
//! `human.c` is not what a straightforward port would guess.

use std::ffi::OsString;

/// gnulib's `strtol_error`, which decides the wording of a bad argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Invalid,
    InvalidSuffix,
    Overflow,
}

/// `human_*` option bits from gnulib's `human.h`. Ceiling is zero there,
/// which is why du rounds up without ever asking for it.
pub const GROUP_DIGITS: u32 = 4;
pub const SUPPRESS_POINT_ZERO: u32 = 8;
pub const AUTOSCALE: u32 = 16;
pub const BASE_1024: u32 = 32;
pub const SI: u32 = 128;
pub const B: u32 = 256;

const POWER_LETTER: [u8; 11] = [
    0, b'K', b'M', b'G', b'T', b'P', b'E', b'Z', b'Y', b'R', b'Q',
];
const EXPONENT_MAX: u32 = 10;

/// `strtoumax`/`strtoimax` with base 0, as C does it: leading space, a sign,
/// `0x` for hex and a leading `0` for octal. Returns the value (clamped on
/// overflow, as C does), how many bytes were consumed and whether it
/// overflowed. `None` when no digits were found.
fn strto(text: &[u8], min: i128, max: i128) -> Option<(i128, usize, bool)> {
    let mut at = 0;
    while text.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    let negative = match text.get(at) {
        Some(b'-') => {
            at += 1;
            true
        }
        Some(b'+') => {
            at += 1;
            false
        }
        _ => false,
    };
    let (radix, digits_at) = match (text.get(at), text.get(at + 1)) {
        (Some(b'0'), Some(b'x' | b'X'))
            if text.get(at + 2).is_some_and(u8::is_ascii_hexdigit) =>
        {
            (16, at + 2)
        }
        (Some(b'0'), _) => (8, at),
        _ => (10, at),
    };
    let mut end = digits_at;
    let mut value: i128 = 0;
    let mut overflow = false;
    while let Some(digit) =
        text.get(end).and_then(|&b| char::from(b).to_digit(radix))
    {
        value = value * i128::from(radix) + i128::from(digit);
        if value > max.max(-min) {
            overflow = true;
            value = max.max(-min);
        }
        end += 1;
    }
    if end == digits_at {
        return None;
    }
    let mut value = if negative { -value } else { value };
    // C's strtoumax negates in unsigned arithmetic; xstrtoumax refuses a
    // minus sign before it gets that far, so only the signed case clamps.
    if value > max {
        value = max;
        overflow = true;
    } else if value < min {
        value = min;
        overflow = true;
    }
    Some((value, end, overflow))
}

/// Multiply, saturating at the type's bounds as gnulib's `bkm_scale` does.
const fn scale(value: &mut i128, by: i128, min: i128, max: i128) -> bool {
    let scaled = *value * by;
    if scaled > max {
        *value = max;
        true
    } else if scaled < min {
        *value = min;
        true
    } else {
        *value = scaled;
        false
    }
}

/// gnulib's `xstrtol` core, for a type bounded by `min..=max`.
/// gnulib's `xstrtol` core, for a type bounded by `min..=max`. Like C, it
/// stores a value even when it reports an error: the number as far as it
/// was understood. `humblock` relies on that.
fn xstrto(
    text: &[u8],
    suffixes: &[u8],
    min: i128,
    max: i128,
) -> (i128, Option<ParseError>) {
    if min == 0 {
        let first = text.iter().find(|b| !b.is_ascii_whitespace());
        if first == Some(&b'-') {
            return (0, Some(ParseError::Invalid));
        }
    }
    let (mut value, end, mut overflow) = match strto(text, min, max) {
        Some(parsed) => parsed,
        None => {
            // No number but a valid suffix means one of that unit.
            match text.first() {
                Some(first) if suffixes.contains(first) => (1, 0, false),
                _ => return (0, Some(ParseError::Invalid)),
            }
        }
    };
    let rest = &text[end..];
    let Some(&unit) = rest.first() else {
        return (value, overflow.then_some(ParseError::Overflow));
    };
    if !suffixes.contains(&unit) {
        return (value, Some(ParseError::InvalidSuffix));
    }
    let mut base = 1024;
    let mut consumed = 1;
    if b"EGgkKMmPQRTtYZ".contains(&unit) && suffixes.contains(&b'0') {
        match rest.get(1) {
            Some(b'i') if rest.get(2) == Some(&b'B') => consumed += 2,
            Some(b'B' | b'D') => {
                base = 1000;
                consumed += 1;
            }
            _ => {}
        }
    }
    let power = match unit {
        b'b' => {
            overflow |= scale(&mut value, 512, min, max);
            0
        }
        b'B' => {
            overflow |= scale(&mut value, 1024, min, max);
            0
        }
        b'c' => 0,
        b'w' => {
            overflow |= scale(&mut value, 2, min, max);
            0
        }
        b'k' | b'K' => 1,
        b'M' | b'm' => 2,
        b'G' | b'g' => 3,
        b'T' | b't' => 4,
        b'P' => 5,
        b'E' => 6,
        b'Z' => 7,
        b'Y' => 8,
        b'R' => 9,
        b'Q' => 10,
        _ => return (value, Some(ParseError::InvalidSuffix)),
    };
    for _ in 0..power {
        overflow |= scale(&mut value, base, min, max);
    }
    if rest.len() > consumed {
        return (value, Some(ParseError::InvalidSuffix));
    }
    (value, overflow.then_some(ParseError::Overflow))
}

fn fail_or<T>(value: T, error: Option<ParseError>) -> Result<T, ParseError> {
    error.map_or(Ok(value), Err)
}

/// `xstrtoumax`: the value, and the error if there was one.
fn xstrtoumax_raw(text: &[u8], suffixes: &[u8]) -> (u64, Option<ParseError>) {
    let (value, error) = xstrto(text, suffixes, 0, i128::from(u64::MAX));
    (u64::try_from(value).unwrap_or(u64::MAX), error)
}

#[cfg(test)]
fn xstrtoumax(text: &[u8], suffixes: &[u8]) -> Result<u64, ParseError> {
    let (value, error) = xstrtoumax_raw(text, suffixes);
    fail_or(value, error)
}

pub fn xstrtoimax(text: &[u8], suffixes: &[u8]) -> Result<i64, ParseError> {
    let (value, error) =
        xstrto(text, suffixes, i128::from(i64::MIN), i128::from(i64::MAX));
    fail_or(i64::try_from(value).unwrap_or(i64::MAX), error)
}

/// The position just past what `xstrtoumax` consumed, for `humblock`'s
/// "was there a digit before the unit" test.
fn consumed_len(text: &[u8], suffixes: &[u8]) -> usize {
    let end = strto(text, 0, i128::from(u64::MAX)).map_or(0, |(_, end, _)| end);
    let rest = &text[end..];
    match rest.first() {
        Some(unit) if suffixes.contains(unit) => {
            let mut consumed = 1;
            if b"EGgkKMmPQRTtYZ".contains(unit) && suffixes.contains(&b'0') {
                match rest.get(1) {
                    Some(b'i') if rest.get(2) == Some(&b'B') => consumed += 2,
                    Some(b'B' | b'D') => consumed += 1,
                    _ => {}
                }
            }
            end + consumed
        }
        _ => end,
    }
}

/// gnulib's `argmatch`: an exact name, or an unambiguous prefix. Prefixes
/// naming several entries count only when they all mean the same thing.
pub fn argmatch<T: Copy + PartialEq>(
    arg: &[u8],
    names: &[(&str, T)],
) -> Result<T, bool> {
    let mut found: Option<T> = None;
    let mut ambiguous = false;
    for &(name, value) in names {
        if name.as_bytes() == arg {
            return Ok(value);
        }
        if name.as_bytes().starts_with(arg) {
            match found {
                None => found = Some(value),
                Some(seen) if seen != value => ambiguous = true,
                Some(_) => {}
            }
        }
    }
    match found {
        Some(value) if !ambiguous => Ok(value),
        // `Err(true)` is "ambiguous", `Err(false)` is "no match".
        _ => Err(ambiguous),
    }
}

/// How sizes are printed: `human_output_opts` and `output_block_size`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Units {
    pub opts: u32,
    pub block_size: u64,
}

impl Units {
    pub const fn plain(block_size: u64) -> Self {
        Self {
            opts: 0,
            block_size,
        }
    }
}

fn default_block_size() -> u64 {
    if std::env::var_os("POSIXLY_CORRECT").is_some() {
        512
    } else {
        1024
    }
}

/// gnulib's `human_options`: `spec` is `-B`'s argument or `DU_BLOCK_SIZE`,
/// with `BLOCK_SIZE` and `BLOCKSIZE` as the fallbacks when it is absent.
pub fn human_options(spec: Option<&[u8]>) -> (Units, Result<(), ParseError>) {
    let env =
        |name: &str| std::env::var_os(name).map(OsString::into_encoded_bytes);
    let owned;
    let spec = if let Some(spec) = spec {
        Some(spec)
    } else {
        owned = env("BLOCK_SIZE").or_else(|| env("BLOCKSIZE"));
        owned.as_deref()
    };
    let (units, result) = match spec {
        None => (Units::plain(default_block_size()), Ok(())),
        Some(spec) => humblock(spec),
    };
    if units.block_size == 0 {
        return (
            Units {
                opts: units.opts,
                block_size: default_block_size(),
            },
            Err(ParseError::Invalid),
        );
    }
    (units, result)
}

fn humblock(spec: &[u8]) -> (Units, Result<(), ParseError>) {
    const SUFFIXES: &[u8] = b"eEgGkKmMpPtTyYzZ0";
    let mut opts = 0;
    let mut spec = spec;
    if let Some(rest) = spec.strip_prefix(b"'") {
        opts |= GROUP_DIGITS;
        spec = rest;
    }
    let names = [
        ("human-readable", AUTOSCALE | SI | BASE_1024),
        ("si", AUTOSCALE | SI),
    ];
    if let Ok(named) = argmatch(spec, &names) {
        return (
            Units {
                opts: opts | named,
                block_size: 1,
            },
            Ok(()),
        );
    }
    // On a bad spec gnulib keeps what `xstrtoumax` understood of it and
    // drops the options, so `DU_BLOCK_SIZE=1x` still means one byte.
    let (block_size, error) = xstrtoumax_raw(spec, SUFFIXES);
    if let Some(error) = error {
        return (Units::plain(block_size), Err(error));
    }
    // A unit with no digit before it ("K", "MiB") also names the suffix
    // the sizes are printed with.
    let end = consumed_len(spec, SUFFIXES);
    if !spec[..end].iter().any(u8::is_ascii_digit) && end > 0 {
        opts |= SI;
        let last = spec[end - 1];
        if last == b'B' {
            opts |= B;
        }
        if last != b'B' || (end >= 2 && spec[end - 2] == b'i') {
            opts |= BASE_1024;
        }
    }
    (Units { opts, block_size }, Ok(()))
}

/// gnulib's `human_readable(n, buf, opts, 1, to_block_size)`, with the
/// ceiling rounding du gets by default. The arithmetic wraps where C's
/// unsigned arithmetic would, so enormous block sizes misprint the same way.
pub fn human_readable(n: u64, units: Units) -> String {
    if n == u64::MAX {
        return "Infinity".to_owned();
    }
    let opts = units.opts;
    let to_block_size = units.block_size.max(1);
    let base: u64 = if opts & BASE_1024 != 0 { 1024 } else { 1000 };
    let (mut amt, mut tenths, mut rounding) = if to_block_size <= 1 {
        (n, 0_u64, 0_u64)
    } else {
        let divisor = to_block_size;
        let r10 = (n % divisor).wrapping_mul(10);
        let r2 = (r10 % divisor).wrapping_mul(2);
        let rounding = if r2 < divisor {
            u64::from(r2 > 0)
        } else {
            2 + u64::from(divisor < r2)
        };
        (n / divisor, r10 / divisor, rounding)
    };
    let mut exponent: i64 = -1;
    let mut fraction = String::new();
    if opts & AUTOSCALE != 0 {
        exponent = 0;
        if base <= amt {
            loop {
                let r10 = (amt % base) * 10 + tenths;
                let r2 = (r10 % base) * 2 + (rounding >> 1);
                amt /= base;
                tenths = r10 / base;
                rounding = if r2 < base {
                    u64::from(r2 + rounding != 0)
                } else {
                    2 + u64::from(base < r2 + rounding)
                };
                exponent += 1;
                if !(base <= amt && exponent < i64::from(EXPONENT_MAX)) {
                    break;
                }
            }
            if amt < 10 {
                if rounding > 0 {
                    tenths += 1;
                    rounding = 0;
                    if tenths == 10 {
                        amt += 1;
                        tenths = 0;
                    }
                }
                if amt < 10 && (tenths != 0 || opts & SUPPRESS_POINT_ZERO == 0)
                {
                    fraction = format!(".{tenths}");
                    tenths = 0;
                    rounding = 0;
                }
            }
        }
    }
    if tenths + rounding > 0 {
        amt += 1;
        if opts & AUTOSCALE != 0
            && amt == base
            && exponent < i64::from(EXPONENT_MAX)
        {
            exponent += 1;
            if opts & SUPPRESS_POINT_ZERO == 0 {
                ".0".clone_into(&mut fraction);
            }
            amt = 1;
        }
    }
    // Digit grouping follows the locale's thousands separator, which is
    // empty in the C locale; this port always uses the C locale.
    let mut out = format!("{amt}{fraction}");
    if opts & SI != 0 {
        if exponent < 0 {
            exponent = 0;
            let mut power: u64 = 1;
            while power < to_block_size {
                exponent += 1;
                if exponent == i64::from(EXPONENT_MAX) {
                    break;
                }
                power = power.wrapping_mul(base);
            }
        }
        if exponent > 0 {
            let letter = if opts & BASE_1024 == 0 && exponent == 1 {
                b'k'
            } else {
                POWER_LETTER[exponent as usize]
            };
            out.push(char::from(letter));
        }
        if opts & B != 0 {
            if opts & BASE_1024 != 0 && exponent > 0 {
                out.push('i');
            }
            out.push('B');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUMAN: Units = Units {
        opts: AUTOSCALE | SI | BASE_1024,
        block_size: 1,
    };

    #[test]
    fn human_sizes_round_up_like_gnu() {
        let cases = [
            (0, "0"),
            (1, "1"),
            (1023, "1023"),
            (1024, "1.0K"),
            (1025, "1.1K"),
            (10_240, "10K"),
            (10_241, "11K"),
            (1_048_575, "1.0M"),
            (1_048_576, "1.0M"),
            (1_048_577, "1.1M"),
            (1024 * 1023 + 1, "1.0M"),
        ];
        for (n, want) in cases {
            assert_eq!(human_readable(n, HUMAN), want, "{n}");
        }
    }

    #[test]
    fn plain_blocks_round_up() {
        assert_eq!(human_readable(0, Units::plain(1024)), "0");
        assert_eq!(human_readable(1, Units::plain(1024)), "1");
        assert_eq!(human_readable(4096, Units::plain(1024)), "4");
        assert_eq!(human_readable(4097, Units::plain(1024)), "5");
    }

    #[test]
    fn block_size_units() {
        let parse = |spec: &str| humblock(spec.as_bytes()).0;
        assert_eq!(parse("1K"), Units::plain(1024));
        assert_eq!(
            parse("K"),
            Units {
                opts: SI | BASE_1024,
                block_size: 1024
            }
        );
        assert_eq!(
            parse("KB"),
            Units {
                opts: SI | B,
                block_size: 1000
            }
        );
        assert_eq!(
            parse("MiB"),
            Units {
                opts: SI | B | BASE_1024,
                block_size: 1 << 20
            }
        );
        assert_eq!(human_readable(5000, parse("K")), "5K");
        assert_eq!(human_readable(5000, parse("KB")), "5kB");
    }

    #[test]
    fn suffix_parsing() {
        assert_eq!(xstrtoimax(b"-2K", b"kKmMGTPEZYRQ0"), Ok(-2048));
        assert_eq!(xstrtoimax(b"1MB", b"kKmMGTPEZYRQ0"), Ok(1_000_000));
        assert_eq!(
            xstrtoimax(b"1x", b"kKmMGTPEZYRQ0"),
            Err(ParseError::InvalidSuffix)
        );
        assert_eq!(xstrtoimax(b"0x10", b""), Ok(16));
        assert_eq!(xstrtoimax(b"010", b""), Ok(8));
        assert_eq!(xstrtoumax(b"-1", b""), Err(ParseError::Invalid));
        assert_eq!(humblock(b"1x").0, Units::plain(1));
        assert_eq!(humblock(b"16Z").1, Err(ParseError::Overflow));
    }
}
