//! What du prints: GNU's lines, or the JSON document `--json` asks for.

use std::fmt::Write as _;
use std::io::{BufWriter, Stdout, Write as _};

use chrono::{DateTime, Local};
use disktree_core::classify::Reclaim;

use crate::num::{Units, human_readable};
use crate::report::{Dui, Kind, Line, Sink};
use crate::walk::Time;

/// `show_date`: strftime with GNU's `%N` for nanoseconds, in the local zone
/// (which honours `TZ`). A format chrono cannot render falls back to the
/// seconds, as du falls back when `strftime` fails.
pub fn format_time(time: Time, format: &str) -> String {
    let Some(utc) = DateTime::from_timestamp(time.sec, time.nsec) else {
        return time.sec.to_string();
    };
    let local = utc.with_timezone(&Local);
    let mut expanded = String::with_capacity(format.len());
    let mut chars = format.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '%' {
            expanded.push(c);
            continue;
        }
        // Collect the conversion's flags and width to see whether it is
        // `%N`, which chrono does not have.
        let mut spec = String::from("%");
        while let Some(&(_, next)) = chars.peek() {
            spec.push(next);
            chars.next();
            if !(next.is_ascii_digit() || "-_0^#:".contains(next)) {
                break;
            }
        }
        if spec.ends_with('N') {
            let width: usize = spec
                .trim_start_matches('%')
                .trim_end_matches('N')
                .trim_start_matches(['-', '_', '0', '^', '#'])
                .parse()
                .unwrap_or(9)
                .clamp(1, 9);
            let digits = format!("{:09}", time.nsec);
            expanded.push_str(&digits[..width]);
        } else {
            expanded.push_str(&spec);
        }
    }
    let mut out = String::new();
    if std::fmt::Write::write_fmt(
        &mut out,
        format_args!("{}", local.format(&expanded)),
    )
    .is_err()
    {
        return time.sec.to_string();
    }
    out
}

/// Standard output failed: end as GNU du does. A reader that went away
/// (`| head`) would have killed it with SIGPIPE, which a shell reports as
/// 141; anything else is "write error" and status 1. The index being written
/// alongside is a cache and can be abandoned.
pub fn write_failed(program: &str, error: &std::io::Error) -> ! {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(141);
    }
    eprintln!("{program}: write error: {}", crate::strerror(error));
    std::process::exit(1);
}

/// GNU's output, line by line.
pub struct Text {
    out: BufWriter<Stdout>,
    program: String,
    units: Units,
    inodes: bool,
    time_format: Option<String>,
    terminator: u8,
}

impl Text {
    pub fn new(
        program: &str,
        units: Units,
        inodes: bool,
        time_format: Option<String>,
        null: bool,
    ) -> Self {
        Self {
            out: BufWriter::with_capacity(64 * 1024, std::io::stdout()),
            program: program.to_owned(),
            units,
            inodes,
            time_format,
            terminator: if null { 0 } else { b'\n' },
        }
    }

    fn write(&mut self, dui: &Dui, path: &[u8]) {
        let value = if self.inodes { dui.inodes } else { dui.size };
        let mut line = human_readable(value, self.units).into_bytes();
        if let Some(format) = &self.time_format {
            line.push(b'\t');
            let time = dui.tmax.unwrap_or(Time {
                sec: i64::MIN,
                nsec: 0,
            });
            line.extend_from_slice(format_time(time, format).as_bytes());
        }
        line.push(b'\t');
        line.extend_from_slice(path);
        line.push(self.terminator);
        if let Err(error) = self.out.write_all(&line) {
            write_failed(&self.program, &error);
        }
    }
}

impl Sink for Text {
    fn line(&mut self, line: &Line<'_>) {
        self.write(&line.dui, line.path);
    }

    fn total(&mut self, dui: &Dui) {
        self.write(dui, b"total");
    }

    fn flush(&mut self) {
        if let Err(error) = self.out.flush() {
            write_failed(&self.program, &error);
        }
    }
}

struct Record {
    path: String,
    size: String,
    value: u64,
    entry_type: &'static str,
    kind: Kind,
    clean: Option<String>,
    last_write: Option<Time>,
}

/// `--json`: the same entries, as one document an agent can read.
pub struct Json {
    units: Units,
    inodes: bool,
    records: Vec<Record>,
    total: Option<Record>,
}

impl Json {
    pub const fn new(units: Units, inodes: bool) -> Self {
        Self {
            units,
            inodes,
            records: Vec::new(),
            total: None,
        }
    }

    fn record(
        &self,
        dui: &Dui,
        path: &[u8],
        line: Option<&Line<'_>>,
    ) -> Record {
        let value = if self.inodes { dui.inodes } else { dui.size };
        let entry_type =
            line.and_then(|line| line.meta).map_or("total", |meta| {
                use rustix::fs::FileType;
                match meta.file_type() {
                    FileType::Directory => "directory",
                    FileType::RegularFile => "file",
                    FileType::Symlink => "symlink",
                    _ => "other",
                }
            });
        let kind = line.map(|line| line.kind).unwrap_or_default();
        Record {
            path: String::from_utf8_lossy(path).into_owned(),
            size: human_readable(value, self.units),
            value,
            entry_type,
            kind,
            clean: line.and_then(|line| clean_command(line.path, kind)),
            last_write: dui.mtime_max,
        }
    }

    /// Write the document. `as_of` is when the numbers were true of the
    /// disk; `source` says whether they came from a walk or the index.
    pub fn write(
        self,
        as_of: std::time::SystemTime,
        source: &str,
    ) -> std::io::Result<()> {
        let as_of: DateTime<Local> = as_of.into();
        let mut out = String::from("{\n");
        let _ = write!(
            out,
            "  \"as_of\": {},\n  \"source\": {},\n",
            json_string(&as_of.to_rfc3339()),
            json_string(source)
        );
        let _ = write!(
            out,
            "  \"unit\": {},\n  \"entries\": [",
            json_string(if self.inodes { "inodes" } else { "bytes" })
        );
        for (at, record) in self.records.iter().enumerate() {
            out.push_str(if at == 0 { "\n" } else { ",\n" });
            out.push_str("    ");
            out.push_str(&record_json(record));
        }
        out.push_str(if self.records.is_empty() {
            "]"
        } else {
            "\n  ]"
        });
        if let Some(total) = &self.total {
            out.push_str(",\n  \"total\": ");
            out.push_str(&record_json(total));
        }
        out.push_str("\n}\n");
        std::io::stdout().write_all(out.as_bytes())
    }
}

impl Sink for Json {
    fn line(&mut self, line: &Line<'_>) {
        let record = self.record(&line.dui, line.path, Some(line));
        self.records.push(record);
    }

    fn total(&mut self, dui: &Dui) {
        self.total = Some(self.record(dui, b"total", None));
    }

    fn flush(&mut self) {}
}

fn record_json(record: &Record) -> String {
    let optional = |value: Option<String>| {
        value.map_or_else(|| "null".to_owned(), |value| json_string(&value))
    };
    let last_write = record.last_write.and_then(|time| {
        DateTime::from_timestamp(time.sec, time.nsec)
            .map(|utc| utc.with_timezone(&Local).to_rfc3339())
    });
    format!(
        "{{\"path\": {}, \"size\": {}, \"value\": {}, \"type\": {}, \
         \"kind\": {}, \"reclaim\": {}, \"clean\": {}, \"last_write\": {}}}",
        json_string(&record.path),
        json_string(&record.size),
        record.value,
        json_string(record.entry_type),
        optional(
            (record.entry_type != "total").then(|| record
                .kind
                .category
                .label()
                .to_owned())
        ),
        optional(record.kind.reclaim.map(|r| r.label().to_owned())),
        optional(record.clean.clone()),
        optional(last_write),
    )
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A path as one shell word.
fn shell_word(path: &[u8]) -> String {
    let text = String::from_utf8_lossy(path);
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "%+,-./:=@_".contains(c))
    {
        return text.into_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The command that gives the space back, when the owning tool has one.
/// Prefer a tool's own clean command to removing its files: it knows what
/// else refers to them. Only offered where the entry's own name decided
/// why it can go, never for what merely sits inside such a directory.
fn clean_command(path: &[u8], kind: Kind) -> Option<String> {
    if !kind.reclaim_here {
        return None;
    }
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    let (parent, name) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
        None => (b".".as_slice(), trimmed),
    };
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    let parent_word = shell_word(if parent.is_empty() { b"/" } else { parent });
    let command = match (kind.reclaim?, name.as_str()) {
        (Reclaim::BuildOutput, "target") => {
            format!("cargo clean --manifest-path {parent_word}/Cargo.toml")
        }
        (Reclaim::PackageStore, _) => "pnpm store prune".to_owned(),
        (Reclaim::Regenerable, "npm-cache" | "_cacache") => {
            "npm cache clean --force".to_owned()
        }
        (Reclaim::Reinstallable | Reclaim::BuildOutput, _) => {
            format!("rm -rf {}", shell_word(trimmed))
        }
        _ => return None,
    };
    Some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanoseconds_and_fallback() {
        let time = Time {
            sec: 0,
            nsec: 123_456_789,
        };
        // The date depends on TZ; the fraction does not.
        assert!(format_time(time, "%S.%N").ends_with(".123456789"));
        assert!(format_time(time, "%3N").ends_with("123"));
    }

    #[test]
    fn json_escapes() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }

    #[test]
    fn clean_commands_prefer_the_tool() {
        let kind = Kind {
            reclaim: Some(Reclaim::BuildOutput),
            reclaim_here: true,
            ..Kind::default()
        };
        assert_eq!(
            clean_command(b"proj/target", kind).as_deref(),
            Some("cargo clean --manifest-path proj/Cargo.toml")
        );
        assert_eq!(
            clean_command(b"a b/__pycache__", kind).as_deref(),
            Some("rm -rf 'a b/__pycache__'")
        );
        let inherited = Kind {
            reclaim_here: false,
            ..kind
        };
        assert_eq!(clean_command(b"proj/target/debug", inherited), None);
    }
}
