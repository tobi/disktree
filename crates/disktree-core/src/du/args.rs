//! The command line, parsed the way GNU du's `main` parses it: glibc's
//! `getopt_long` (argument permutation, unique-prefix long options and its
//! exact complaints), then each option applied in order, since several of
//! them overwrite what an earlier one set.
//!
//! disktree's own options are matched only when spelled out in full, so they
//! can never make an abbreviation that GNU accepts ambiguous.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use super::exclude::Excludes;
use super::num::{self, ParseError, Units};
use super::quote;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deref {
    /// `-P`, the default: never follow a symbolic link.
    Physical,
    /// `-D`/`-H`: follow the ones named on the command line.
    Args,
    /// `-L`: follow every one.
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeKind {
    Modified,
    Accessed,
    Changed,
}

/// Something to measure, or a complaint to make in its place, in the order
/// the operands were given: du reports a bad one where it stands.
#[derive(Clone, Debug)]
pub enum Operand {
    Path(Vec<u8>),
    Invalid(String),
}

#[derive(Debug)]
pub struct Options {
    pub all: bool,
    pub apparent: bool,
    pub units: Units,
    pub total: bool,
    pub deref: Deref,
    pub max_depth: i64,
    pub inodes: bool,
    pub count_links: bool,
    pub separate_dirs: bool,
    pub threshold: i64,
    pub excludes: Excludes,
    pub one_file_system: bool,
    pub null: bool,
    /// `--time`, and the strftime format it prints with.
    pub time: Option<(TimeKind, String)>,
    pub operands: Vec<Operand>,
    /// Whether every dev/ino pair is remembered, not only multiply-linked
    /// files': more than one operand, or `-L`, can meet a file twice.
    pub hash_all: bool,
    /// disktree's additions.
    pub json: bool,
    pub fresh: bool,
    pub max_age: Option<Duration>,
    pub index: bool,
}

/// What the command line asks for: a run, or an exit it already explained.
#[derive(Debug)]
pub enum Parsed {
    Run(Box<Options>),
    Exit(i32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HasArg {
    No,
    Required,
    Optional,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Opt {
    Short(u8),
    Exclude,
    Files0From,
    Si,
    Time,
    TimeStyle,
    Inodes,
    Help,
    Version,
    Json,
    Fresh,
    MaxAge,
    NoIndex,
}

/// GNU du's `long_options`, in its order: an ambiguous prefix lists its
/// candidates in this order.
const LONG: &[(&str, HasArg, Opt)] = &[
    ("all", HasArg::No, Opt::Short(b'a')),
    ("apparent-size", HasArg::No, Opt::Short(b'A')),
    ("block-size", HasArg::Required, Opt::Short(b'B')),
    ("bytes", HasArg::No, Opt::Short(b'b')),
    ("count-links", HasArg::No, Opt::Short(b'l')),
    ("dereference", HasArg::No, Opt::Short(b'L')),
    ("dereference-args", HasArg::No, Opt::Short(b'D')),
    ("exclude", HasArg::Required, Opt::Exclude),
    ("exclude-from", HasArg::Required, Opt::Short(b'X')),
    ("files0-from", HasArg::Required, Opt::Files0From),
    ("human-readable", HasArg::No, Opt::Short(b'h')),
    ("inodes", HasArg::No, Opt::Inodes),
    ("si", HasArg::No, Opt::Si),
    ("max-depth", HasArg::Required, Opt::Short(b'd')),
    ("null", HasArg::No, Opt::Short(b'0')),
    ("no-dereference", HasArg::No, Opt::Short(b'P')),
    ("one-file-system", HasArg::No, Opt::Short(b'x')),
    ("separate-dirs", HasArg::No, Opt::Short(b'S')),
    ("summarize", HasArg::No, Opt::Short(b's')),
    ("total", HasArg::No, Opt::Short(b'c')),
    ("threshold", HasArg::Required, Opt::Short(b't')),
    ("time", HasArg::Optional, Opt::Time),
    ("time-style", HasArg::Required, Opt::TimeStyle),
    ("help", HasArg::No, Opt::Help),
    ("version", HasArg::No, Opt::Version),
];

/// disktree's options: exact names only.
const EXTENSIONS: &[(&str, HasArg, Opt)] = &[
    ("json", HasArg::No, Opt::Json),
    ("fresh", HasArg::No, Opt::Fresh),
    ("max-age", HasArg::Required, Opt::MaxAge),
    ("no-index", HasArg::No, Opt::NoIndex),
];

/// `getopt_long`'s short option string for du; `:` marks an argument.
const SHORT: &[u8] = b"0aAbd:chHklmst:xB:DLPSX:";

fn short_takes_arg(c: u8) -> Option<bool> {
    let at = SHORT.iter().position(|&s| s == c && c != b':')?;
    Some(SHORT.get(at + 1) == Some(&b':'))
}

const TIME_WORDS: &[(&str, TimeKind)] = &[
    ("atime", TimeKind::Accessed),
    ("access", TimeKind::Accessed),
    ("use", TimeKind::Accessed),
    ("ctime", TimeKind::Changed),
    ("status", TimeKind::Changed),
];

const TIME_STYLES: &[(&str, &str)] = &[
    ("full-iso", "%Y-%m-%d %H:%M:%S.%N %z"),
    ("long-iso", "%Y-%m-%d %H:%M"),
    ("iso", "%Y-%m-%d"),
];

/// The two names GNU du goes by. `error()` prefixes its messages with the
/// basename of `argv[0]`; `getopt`'s complaints, the "Try ... --help" line
/// and the usage text repeat `argv[0]` exactly as it was given.
#[derive(Clone, Debug)]
pub struct Program {
    pub short: String,
    pub full: String,
}

impl Program {
    pub fn new(argv0: &[u8]) -> Self {
        let argv0 = if argv0.is_empty() {
            b"du".as_slice()
        } else {
            argv0
        };
        let base = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
        Self {
            short: String::from_utf8_lossy(base).into_owned(),
            full: String::from_utf8_lossy(argv0).into_owned(),
        }
    }
}

struct Parser<'a> {
    program: &'a Program,
    ok: bool,
}

impl Parser<'_> {
    fn error(&self, message: &str) {
        eprintln!("{}: {message}", self.program.short);
    }

    fn try_help(&self) {
        eprintln!("Try '{} --help' for more information.", self.program.full);
    }

    fn fail(&mut self, message: &str) {
        self.error(message);
        self.ok = false;
    }
}

/// What `getopt_long` returns, in order: an option, or its complaint about
/// one, which it prints under the full `argv[0]` as it meets it.
enum Item {
    Given(Given),
    Complaint(String),
}

fn bytes(arg: &OsString) -> Vec<u8> {
    arg.clone().into_encoded_bytes()
}

/// One option as `getopt_long` hands it over: what it is, its argument,
/// and the long name it came from, which the complaints about a bad
/// argument repeat.
struct Given {
    opt: Opt,
    arg: Option<Vec<u8>>,
    long: Option<&'static str>,
}

fn option_label(given: &Given) -> String {
    match (given.long, given.opt) {
        (Some(name), _) => format!("--{name}"),
        (None, Opt::Short(c)) => format!("-{}", char::from(c)),
        (None, _) => String::new(),
    }
}

fn strtol_fatal(parser: &Parser<'_>, error: ParseError, given: &Given) {
    let label = option_label(given);
    let arg = String::from_utf8_lossy(given.arg.as_deref().unwrap_or(b""));
    let message = match error {
        ParseError::Invalid => format!("invalid {label} argument '{arg}'"),
        ParseError::InvalidSuffix => {
            format!("invalid suffix in {label} argument '{arg}'")
        }
        ParseError::Overflow => format!("{label} argument '{arg}' too large"),
    };
    parser.error(&message);
}

/// Split the command line into options and operands, as glibc's
/// `getopt_long` does with its default argument permutation.
fn scan(args: &[OsString]) -> (Vec<Item>, Vec<Vec<u8>>) {
    let posix = std::env::var_os("POSIXLY_CORRECT").is_some();
    let mut given = Vec::new();
    let mut operands = Vec::new();
    let mut at = 0;
    let mut done = false;
    while at < args.len() {
        let arg = bytes(&args[at]);
        at += 1;
        if done || arg == b"-" || !arg.starts_with(b"-") {
            operands.push(arg);
            if posix {
                done = true;
            }
            continue;
        }
        if arg == b"--" {
            done = true;
            continue;
        }
        if let Some(body) = arg.strip_prefix(b"--") {
            let (name, value) = match body.iter().position(|&b| b == b'=') {
                Some(eq) => (&body[..eq], Some(body[eq + 1..].to_vec())),
                None => (body, None),
            };
            let exact = LONG
                .iter()
                .chain(EXTENSIONS)
                .find(|(long, ..)| long.as_bytes() == name);
            let found = if let Some(&found) = exact {
                found
            } else {
                let matches: Vec<_> = LONG
                    .iter()
                    .filter(|(long, ..)| long.as_bytes().starts_with(name))
                    .collect();
                match matches.as_slice() {
                    [] => {
                        given.push(Item::Complaint(format!(
                            "unrecognized option '{}'",
                            String::from_utf8_lossy(&arg)
                        )));
                        continue;
                    }
                    [only] => **only,
                    [first, rest @ ..]
                        if rest
                            .iter()
                            .all(|m| m.1 == first.1 && m.2 == first.2) =>
                    {
                        **first
                    }
                    many => {
                        let list: Vec<String> = many
                            .iter()
                            .map(|(long, ..)| format!("'--{long}'"))
                            .collect();
                        // glibc repeats the option as given, `=value` too.
                        given.push(Item::Complaint(format!(
                            "option '--{}' is ambiguous; possibilities: {}",
                            String::from_utf8_lossy(body),
                            list.join(" ")
                        )));
                        continue;
                    }
                }
            };
            let (long, has_arg, opt) = found;
            let value = match (has_arg, value) {
                (HasArg::No, Some(_)) => {
                    given.push(Item::Complaint(format!(
                        "option '--{long}' doesn't allow an argument"
                    )));
                    continue;
                }
                (HasArg::Required, None) => {
                    if let Some(next) = args.get(at) {
                        at += 1;
                        Some(bytes(next))
                    } else {
                        given.push(Item::Complaint(format!(
                            "option '--{long}' requires an argument"
                        )));
                        continue;
                    }
                }
                (_, value) => value,
            };
            given.push(Item::Given(Given {
                opt,
                arg: value,
                long: Some(long),
            }));
            continue;
        }
        let mut chars = 1;
        while chars < arg.len() {
            let c = arg[chars];
            chars += 1;
            match short_takes_arg(c) {
                None => {
                    given.push(Item::Complaint(format!(
                        "invalid option -- '{}'",
                        char::from(c)
                    )));
                }
                Some(false) => given.push(Item::Given(Given {
                    opt: Opt::Short(c),
                    arg: None,
                    long: None,
                })),
                Some(true) => {
                    let value = if chars < arg.len() {
                        let value = arg[chars..].to_vec();
                        chars = arg.len();
                        Some(value)
                    } else if let Some(next) = args.get(at) {
                        at += 1;
                        Some(bytes(next))
                    } else {
                        given.push(Item::Complaint(format!(
                            "option requires an argument -- '{}'",
                            char::from(c)
                        )));
                        None
                    };
                    if let Some(value) = value {
                        given.push(Item::Given(Given {
                            opt: Opt::Short(c),
                            arg: Some(value),
                            long: None,
                        }));
                    }
                }
            }
        }
    }
    (given, operands)
}

fn argmatch_valid<T: Copy + PartialEq>(names: &[(&str, T)]) {
    let mut out = String::from("Valid arguments are:");
    let mut last: Option<T> = None;
    for &(name, value) in names {
        if last == Some(value) {
            let _ = write!(out, ", {}", quote::locale(name.as_bytes()));
        } else {
            let _ = write!(out, "\n  - {}", quote::locale(name.as_bytes()));
        }
        last = Some(value);
    }
    eprintln!("{out}");
}

fn argmatch_invalid(
    parser: &Parser<'_>,
    context: &str,
    value: &[u8],
    ambiguous: bool,
) {
    let problem = if ambiguous { "ambiguous" } else { "invalid" };
    parser.error(&format!(
        "{problem} argument {} for {}",
        quote::locale(value),
        quote::locale(context.as_bytes())
    ));
}

/// A duration for `--max-age`: seconds, or a number with `s`, `m`, `h` or
/// `d`.
fn parse_age(text: &[u8]) -> Option<Duration> {
    let text = std::str::from_utf8(text).ok()?;
    let (digits, unit) = match text.char_indices().last()? {
        (at, c) if c.is_ascii_alphabetic() => (&text[..at], c),
        _ => (text, 's'),
    };
    let value: f64 = digits.parse().ok().filter(|v: &f64| *v >= 0.0)?;
    let seconds = match unit {
        's' => value,
        'm' => value * 60.0,
        'h' => value * 3600.0,
        'd' => value * 86_400.0,
        _ => return None,
    };
    Duration::try_from_secs_f64(seconds).ok()
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name)
        .is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
}

/// Parse `args` (without `argv[0]`) as GNU du would.
pub fn parse(program: &Program, args: &[OsString]) -> Parsed {
    let mut parser = Parser { program, ok: true };
    let du_block_size =
        std::env::var_os("DU_BLOCK_SIZE").map(OsString::into_encoded_bytes);
    // du ignores a bad DU_BLOCK_SIZE: the default stands in silently.
    let (units, _) = num::human_options(du_block_size.as_deref());
    let mut options = Options {
        all: false,
        apparent: false,
        units,
        total: false,
        deref: Deref::Physical,
        max_depth: i64::MAX,
        inodes: false,
        count_links: false,
        separate_dirs: false,
        threshold: 0,
        excludes: Excludes::default(),
        one_file_system: false,
        null: false,
        time: None,
        operands: Vec::new(),
        hash_all: false,
        json: env_flag("DISKTREE_DU_JSON"),
        fresh: env_flag("DISKTREE_DU_FRESH"),
        max_age: std::env::var_os("DISKTREE_DU_MAX_AGE")
            .and_then(|v| parse_age(v.as_encoded_bytes())),
        index: std::env::var_os("DISKTREE_DU_INDEX")
            .is_none_or(|v| v != "0" && v != "false"),
    };
    let mut summarize = false;
    let mut max_depth_given = false;
    let mut files0_from: Option<Vec<u8>> = None;
    let mut time_style: Option<Vec<u8>> = None;
    let (items, operands) = scan(args);

    for item in &items {
        let given = match item {
            Item::Given(given) => given,
            Item::Complaint(message) => {
                eprintln!("{}: {message}", program.full);
                parser.ok = false;
                continue;
            }
        };
        let arg = given.arg.as_deref().unwrap_or_default();
        match given.opt {
            Opt::Short(b'0') => options.null = true,
            Opt::Short(b'a') => options.all = true,
            Opt::Short(b'A') => options.apparent = true,
            Opt::Short(b'b') => {
                options.apparent = true;
                options.units = Units::plain(1);
            }
            Opt::Short(b'c') => options.total = true,
            Opt::Short(b'h') => {
                options.units = Units {
                    opts: num::AUTOSCALE | num::SI | num::BASE_1024,
                    block_size: 1,
                };
            }
            Opt::Si => {
                options.units = Units {
                    opts: num::AUTOSCALE | num::SI,
                    block_size: 1,
                };
            }
            Opt::Short(b'k') => options.units = Units::plain(1024),
            Opt::Short(b'm') => options.units = Units::plain(1024 * 1024),
            Opt::Short(b'd') => match num::xstrtoimax(arg, b"") {
                Ok(depth) => {
                    max_depth_given = true;
                    options.max_depth = depth;
                }
                Err(_) => parser.fail(&format!(
                    "invalid maximum depth {}",
                    quote::locale(arg)
                )),
            },
            Opt::Short(b'l') => options.count_links = true,
            Opt::Short(b's') => summarize = true,
            Opt::Short(b't') => {
                match num::xstrtoimax(arg, b"kKmMGTPEZYRQ0") {
                    Ok(threshold) => options.threshold = threshold,
                    Err(error) => {
                        strtol_fatal(&parser, error, given);
                        return Parsed::Exit(1);
                    }
                }
                if options.threshold == 0 && arg.first() == Some(&b'-') {
                    parser.error("invalid --threshold argument '-0'");
                    return Parsed::Exit(1);
                }
            }
            Opt::Short(b'x') => options.one_file_system = true,
            Opt::Short(b'B') => {
                let (units, result) = num::human_options(Some(arg));
                options.units = units;
                if let Err(error) = result {
                    strtol_fatal(&parser, error, given);
                    return Parsed::Exit(1);
                }
            }
            Opt::Short(b'H' | b'D') => options.deref = Deref::Args,
            Opt::Short(b'L') => options.deref = Deref::All,
            Opt::Short(b'P') => options.deref = Deref::Physical,
            Opt::Short(b'S') => options.separate_dirs = true,
            Opt::Short(b'X') => match read_all(arg) {
                Ok(contents) => options.excludes.add_file(&contents),
                Err(error) => parser.fail(&format!(
                    "{}: {}",
                    quote::when_needed(arg),
                    super::strerror(&error)
                )),
            },
            Opt::Files0From => files0_from = Some(arg.to_vec()),
            Opt::Exclude => options.excludes.add(arg),
            Opt::Inodes => options.inodes = true,
            Opt::Time => {
                let kind = match &given.arg {
                    None => TimeKind::Modified,
                    Some(word) => match num::argmatch(word, TIME_WORDS) {
                        Ok(kind) => kind,
                        Err(ambiguous) => {
                            argmatch_invalid(
                                &parser, "--time", word, ambiguous,
                            );
                            argmatch_valid(TIME_WORDS);
                            parser.try_help();
                            return Parsed::Exit(1);
                        }
                    },
                };
                options.time = Some((kind, String::new()));
            }
            Opt::TimeStyle => time_style = Some(arg.to_vec()),
            Opt::Help => {
                print_help(&program.full);
                return Parsed::Exit(0);
            }
            Opt::Version => {
                println!("du (disktree) {}", env!("CARGO_PKG_VERSION"));
                return Parsed::Exit(0);
            }
            Opt::Json => options.json = true,
            Opt::Fresh => options.fresh = true,
            Opt::NoIndex => options.index = false,
            Opt::MaxAge => match parse_age(arg) {
                Some(age) => options.max_age = Some(age),
                None => parser.fail(&format!(
                    "invalid --max-age argument {}",
                    quote::locale(arg)
                )),
            },
            Opt::Short(_) => parser.ok = false,
        }
    }

    if !parser.ok {
        parser.try_help();
        return Parsed::Exit(1);
    }
    if options.all && summarize {
        parser.error("cannot both summarize and show all entries");
        parser.try_help();
        return Parsed::Exit(1);
    }
    if summarize && max_depth_given && options.max_depth == 0 {
        parser.error("warning: summarizing is the same as using --max-depth=0");
    }
    if summarize && max_depth_given && options.max_depth != 0 {
        parser.error(&format!(
            "warning: summarizing conflicts with --max-depth={}",
            options.max_depth
        ));
        parser.try_help();
        return Parsed::Exit(1);
    }
    if summarize {
        options.max_depth = 0;
    }
    if options.inodes {
        if options.apparent {
            parser.error(
                "warning: options --apparent-size and -b are ineffective \
                 with --inodes",
            );
        }
        options.units.block_size = 1;
    }
    if let Some((kind, _)) = options.time {
        let Some(format) = time_format(&parser, time_style) else {
            return Parsed::Exit(1);
        };
        options.time = Some((kind, format));
    }

    if let Some(from) = files0_from {
        if let Some(extra) = operands.first() {
            parser.error(&format!("extra operand {}", quote::locale(extra)));
            eprintln!("file operands cannot be combined with --files0-from");
            parser.try_help();
            return Parsed::Exit(1);
        }
        // Failing to open the list ends du; failing to read it part way is
        // reported where it happens, and the total is still printed.
        let mut source: Box<dyn std::io::Read> = if from == b"-" {
            Box::new(std::io::stdin())
        } else {
            match std::fs::File::open(bytes_path(&from)) {
                Ok(file) => Box::new(file),
                Err(error) => {
                    parser.error(&format!(
                        "cannot open {} for reading: {}",
                        quote::always(&from),
                        super::strerror(&error)
                    ));
                    return Parsed::Exit(1);
                }
            }
        };
        let mut contents = Vec::new();
        let read_error = source.read_to_end(&mut contents).err();
        let mut names: Vec<&[u8]> = contents.split(|&b| b == 0).collect();
        if names.last().is_some_and(|name| name.is_empty()) {
            names.pop();
        }
        for (number, name) in names.into_iter().enumerate() {
            let operand = if from == b"-" && name == b"-" {
                Operand::Invalid(format!(
                    "when reading file names from standard input, no file \
                     name of {} allowed",
                    quote::always(name)
                ))
            } else if name.is_empty() {
                Operand::Invalid(format!(
                    "{}:{}: invalid zero-length file name",
                    quote::when_needed(&from),
                    number + 1
                ))
            } else {
                Operand::Path(name.to_vec())
            };
            options.operands.push(operand);
        }
        if let Some(error) = read_error {
            options.operands.push(Operand::Invalid(format!(
                "{}: read error: {}",
                quote::when_needed(&from),
                super::strerror(&error)
            )));
        }
        options.hash_all = true;
    } else {
        options.hash_all = operands.len() > 1 || options.deref == Deref::All;
        if operands.is_empty() {
            options.operands.push(Operand::Path(b".".to_vec()));
        }
        for name in operands {
            options.operands.push(if name.is_empty() {
                Operand::Invalid("invalid zero-length file name".to_owned())
            } else {
                Operand::Path(name)
            });
        }
    }
    Parsed::Run(Box::new(options))
}

/// A file's contents, or standard input's for `-`.
fn read_all(name: &[u8]) -> std::io::Result<Vec<u8>> {
    if name == b"-" {
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut buffer)?;
        Ok(buffer)
    } else {
        std::fs::read(bytes_path(name))
    }
}

fn bytes_path(bytes: &[u8]) -> std::path::PathBuf {
    // On Unix any byte string is a path.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The strftime format `--time` prints with, from `--time-style` or the
/// `TIME_STYLE` environment variable, as du resolves them.
fn time_format(parser: &Parser<'_>, given: Option<Vec<u8>>) -> Option<String> {
    let style = if let Some(style) = given {
        style
    } else {
        {
            let env = std::env::var_os("TIME_STYLE")
                .map(OsString::into_encoded_bytes);
            match env {
                None => b"long-iso".to_vec(),
                Some(style) if style == b"locale" => b"long-iso".to_vec(),
                Some(style) if style.starts_with(b"+") => {
                    // Anything after a newline is for ls's recent-files
                    // format, which du has no use for.
                    let end = style
                        .iter()
                        .position(|&b| b == b'\n')
                        .unwrap_or(style.len());
                    style[..end].to_vec()
                }
                Some(mut style) => {
                    while let Some(rest) = style.strip_prefix(b"posix-") {
                        style = rest.to_vec();
                    }
                    style
                }
            }
        }
    };
    if let Some(format) = style.strip_prefix(b"+") {
        return Some(String::from_utf8_lossy(format).into_owned());
    }
    match num::argmatch(&style, TIME_STYLES) {
        Ok(format) => Some(format.to_owned()),
        Err(ambiguous) => {
            argmatch_invalid(parser, "time style", &style, ambiguous);
            eprintln!(
                "Valid arguments are:\n  - full-iso\n  - long-iso\n  - iso\n  \
                 - +FORMAT (e.g., +%H:%M) for a 'date'-style format"
            );
            parser.try_help();
            None
        }
    }
}

fn print_help(program: &str) {
    let text = format!(
        "\
Usage: {program} [OPTION]... [FILE]...
  or:  {program} [OPTION]... --files0-from=F
Summarize device usage of the set of FILEs, recursively for directories.

Mandatory arguments to long options are mandatory for short options too.
  -0, --null            end each output line with NUL, not newline
  -a, --all             write counts for all files, not just directories
  -A, --apparent-size   print apparent sizes rather than device usage
  -B, --block-size=SIZE  scale sizes by SIZE before printing them
  -b, --bytes           equivalent to '--apparent-size --block-size=1'
  -c, --total           produce a grand total
  -D, --dereference-args  dereference only symlinks that are listed on the
                          command line
  -d, --max-depth=N     print the total for a directory (or file, with --all)
                          only if it is N or fewer levels below the command
                          line argument;  --max-depth=0 is the same as
                          --summarize
      --files0-from=F   summarize device usage of the NUL-terminated file
                          names specified in file F; if F is -, then read
                          names from standard input
  -H                    equivalent to --dereference-args (-D)
  -h, --human-readable  print sizes in human readable format (e.g., 1K 234M 2G)
      --inodes          list inode usage information instead of block usage
  -k                    like --block-size=1K
  -L, --dereference     dereference all symbolic links
  -l, --count-links     count sizes many times if hard linked
  -m                    like --block-size=1M
  -P, --no-dereference  don't follow any symbolic links (this is the default)
  -S, --separate-dirs   for directories do not include size of subdirectories
      --si              like -h, but use powers of 1000 not 1024
  -s, --summarize       display only a total for each argument
  -t, --threshold=SIZE  exclude entries smaller than SIZE if positive,
                          or entries greater than SIZE if negative
      --time            show time of the last modification of any file in the
                          directory, or any of its subdirectories
      --time=WORD       show time as WORD instead of modification time:
                          atime, access, use, ctime or status
      --time-style=STYLE  show times using STYLE, which can be:
                            full-iso, long-iso, iso, or +FORMAT;
                            FORMAT is interpreted like in 'date'
  -X, --exclude-from=FILE  exclude files that match any pattern in FILE
      --exclude=PATTERN  exclude files that match PATTERN
  -x, --one-file-system  skip directories on different file systems
      --help        display this help and exit
      --version     output version information and exit

disktree additions (full names only):
      --json            one JSON document: every entry du would print, with
                          its kind, why it can be had back, the command
                          that frees it, and when it was last written
      --fresh           walk everything again and rewrite the index
      --max-age=AGE     when the last walk is younger than AGE (seconds, or
                          30s 10m 2h 1d), bring it up to date from what
                          changed since instead of walking again
      --no-index        neither read nor write the persistent index
  The same through the environment, for use behind a `du` symlink:
  DISKTREE_DU_JSON=1 DISKTREE_DU_FRESH=1 DISKTREE_DU_MAX_AGE=AGE
  DISKTREE_DU_INDEX=0

Display values are in units of the first available SIZE from --block-size,
and the DU_BLOCK_SIZE, BLOCK_SIZE and BLOCKSIZE environment variables.
Otherwise, units default to 1024 bytes (or 512 if POSIXLY_CORRECT is set).
"
    );
    let _ = std::io::stdout().write_all(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Box<Options> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        match parse(&Program::new(b"du"), &args) {
            Parsed::Run(options) => options,
            Parsed::Exit(code) => panic!("exited {code}"),
        }
    }

    #[test]
    fn options_permute_and_cluster() {
        let options = run(&["a", "-sh", "b", "--max-d=0"]);
        assert_eq!(options.max_depth, 0);
        assert!(options.units.opts & num::AUTOSCALE != 0);
        assert_eq!(options.operands.len(), 2);
        assert!(options.hash_all);
    }

    #[test]
    fn later_units_win() {
        assert_eq!(run(&["-h", "-k"]).units, Units::plain(1024));
        assert_eq!(run(&["-b"]).units, Units::plain(1));
        assert!(run(&["-b"]).apparent);
    }

    #[test]
    fn ages() {
        assert_eq!(parse_age(b"90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_age(b"10m"), Some(Duration::from_mins(10)));
        assert_eq!(parse_age(b"x"), None);
    }
}
