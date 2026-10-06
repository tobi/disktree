//! gnulib's quoting for diagnostics. du names files with `quoteaf` (shell
//! escaping, always quoted), a few messages with `quotef` (shell escaping
//! only when needed), and option values with `quote` (the locale's quotes).

/// Whether the locale's character set is UTF-8, which decides both the
/// quote marks and whether non-ASCII names print as themselves.
pub fn utf8_locale() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
        .is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        })
}

/// `quote`: the locale's quotation marks, curly in a UTF-8 locale.
pub fn locale(text: &[u8]) -> String {
    let body = String::from_utf8_lossy(text);
    if utf8_locale() {
        format!("\u{2018}{body}\u{2019}")
    } else {
        format!("'{body}'")
    }
}

/// `quoteaf`: shell-escaped and always quoted.
pub fn always(text: &[u8]) -> String {
    shell(text, true)
}

/// `quotef`: shell-escaped, quoted only when a shell would need it.
pub fn when_needed(text: &[u8]) -> String {
    shell(text, false)
}

enum Piece {
    Literal(char),
    Escape(String),
}

fn pieces(text: &[u8], utf8: bool) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(&byte) = rest.first() {
        if utf8 && byte >= 0x80 {
            let len = match byte {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => 0,
            };
            let decoded = rest
                .get(..len)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .and_then(|s| s.chars().next())
                .filter(|c| !c.is_control());
            if let Some(c) = decoded {
                out.push(Piece::Literal(c));
                rest = &rest[len..];
                continue;
            }
        }
        let piece = match byte {
            0x07 => Piece::Escape("\\a".into()),
            0x08 => Piece::Escape("\\b".into()),
            0x0C => Piece::Escape("\\f".into()),
            b'\n' => Piece::Escape("\\n".into()),
            b'\r' => Piece::Escape("\\r".into()),
            b'\t' => Piece::Escape("\\t".into()),
            0x0B => Piece::Escape("\\v".into()),
            0x20..=0x7E => Piece::Literal(char::from(byte)),
            _ => Piece::Escape(format!("\\{byte:03o}")),
        };
        out.push(piece);
        rest = &rest[1..];
    }
    out
}

fn shell(text: &[u8], always: bool) -> String {
    if text.is_empty() {
        return "''".to_owned();
    }
    let pieces = pieces(text, utf8_locale());
    let has_escape = pieces.iter().any(|p| matches!(p, Piece::Escape(_)));
    let literal = |test: fn(char) -> bool| {
        pieces
            .iter()
            .any(|p| matches!(p, Piece::Literal(c) if test(*c)))
    };
    if !always && !has_escape {
        let safe = pieces.iter().enumerate().all(|(at, piece)| match piece {
            Piece::Literal(c) => {
                c.is_alphanumeric()
                    || "%+,-./:@_]{}".contains(*c)
                    || (at > 0 && (*c == '~' || *c == '#'))
            }
            Piece::Escape(_) => false,
        });
        if safe {
            return String::from_utf8_lossy(text).into_owned();
        }
    }
    // A single quote with nothing a double-quoted shell word would expand
    // reads best inside double quotes, which is what gnulib chooses.
    if !has_escape
        && literal(|c| c == '\'')
        && !literal(|c| matches!(c, '"' | '$' | '`' | '\\' | '!'))
    {
        return format!("\"{}\"", String::from_utf8_lossy(text));
    }
    let mut out = String::from("'");
    let mut escaping = false;
    for piece in pieces {
        match piece {
            Piece::Literal(c) => {
                if escaping {
                    out.push_str("''");
                    escaping = false;
                }
                if c == '\'' {
                    out.push_str("'\\''");
                } else {
                    out.push(c);
                }
            }
            Piece::Escape(escape) => {
                if !escaping {
                    out.push_str("'$'");
                    escaping = true;
                }
                out.push_str(&escape);
            }
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_matches_gnulib() {
        assert_eq!(always(b"a b"), "'a b'");
        assert_eq!(always(b"it's"), "\"it's\"");
        assert_eq!(always(b"it's $x"), "'it'\\''s $x'");
        assert_eq!(always(b"a\nb"), "'a'$'\\n''b'");
        assert_eq!(always(b"\xff"), "''$'\\377'");
        assert_eq!(when_needed(b"plain/path.txt"), "plain/path.txt");
        assert_eq!(when_needed(b"a b"), "'a b'");
    }
}
