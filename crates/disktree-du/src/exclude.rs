//! `--exclude` and `-X`, as gnulib's `exclude.c` applies them for du.
//!
//! du passes `EXCLUDE_WILDCARDS` and nothing else, so a pattern is matched
//! with `fnmatch` and no flags: `*` crosses `/`, a leading `.` is not
//! special, and a backslash escapes. It is tried against the whole path and
//! then against every suffix that starts just after a `/`, which is what
//! lets `--exclude=node_modules` catch `a/b/node_modules`.

#[derive(Clone, Debug, Default)]
pub struct Excludes {
    patterns: Vec<Vec<u8>>,
}

impl Excludes {
    pub fn add(&mut self, pattern: &[u8]) {
        self.patterns.push(pattern.to_vec());
    }

    /// Patterns from a file, one per line, as `-X` reads them: gnulib's
    /// `add_exclude_fp` drops trailing white space, a `\r` included, and
    /// skips lines left empty.
    pub fn add_file(&mut self, contents: &[u8]) {
        for line in contents.split(|&b| b == b'\n') {
            let end = line
                .iter()
                .rposition(|&b| !(b.is_ascii_whitespace() || b == 0x0b))
                .map_or(0, |at| at + 1);
            if end > 0 {
                self.add(&line[..end]);
            }
        }
    }

    pub fn patterns(&self) -> impl Iterator<Item = &[u8]> {
        self.patterns.iter().map(Vec::as_slice)
    }

    pub const fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn excludes(&self, path: &[u8]) -> bool {
        self.patterns.iter().any(|pattern| {
            if fnmatch(pattern, path) {
                return true;
            }
            path.iter().enumerate().any(|(at, &byte)| {
                byte == b'/'
                    && path.get(at + 1).is_some_and(|&next| next != b'/')
                    && fnmatch(pattern, &path[at + 1..])
            })
        })
    }
}

/// POSIX `fnmatch` with no flags, over bytes.
pub fn fnmatch(pattern: &[u8], text: &[u8]) -> bool {
    // Iterative with one backtrack point per `*`, the classic approach: a
    // later star supersedes an earlier one, so one point is enough.
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    while pattern.get(p) == Some(&b'*') {
                        p += 1;
                    }
                    star = Some((p, t));
                    continue;
                }
                b'?' if t < text.len() => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' if t < text.len() => {
                    if let Some((matched, next)) = bracket(pattern, p, text[t])
                    {
                        if matched {
                            p = next;
                            t += 1;
                            continue;
                        }
                    } else if text[t] == b'[' {
                        // An unterminated bracket is an ordinary `[`.
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
                b'\\' if t < text.len() => {
                    let literal = pattern.get(p + 1).copied().unwrap_or(b'\\');
                    if literal == text[t] {
                        p += if p + 1 < pattern.len() { 2 } else { 1 };
                        t += 1;
                        continue;
                    }
                }
                literal
                    if t < text.len()
                        && literal == text[t]
                        && literal != b'?' =>
                {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        } else if t == text.len() {
            return true;
        }
        match star {
            Some((star_p, star_t)) if star_t < text.len() => {
                star = Some((star_p, star_t + 1));
                p = star_p;
                t = star_t + 1;
            }
            _ => return false,
        }
    }
}

/// Match one byte against the bracket expression at `pattern[open]`.
/// Returns whether it matched and where the expression ends, or `None` when
/// the bracket is never closed.
fn bracket(pattern: &[u8], open: usize, byte: u8) -> Option<(bool, usize)> {
    let mut at = open + 1;
    let negate = matches!(pattern.get(at), Some(b'!' | b'^'));
    if negate {
        at += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let &current = pattern.get(at)?;
        if current == b']' && !first {
            return Some((matched != negate, at + 1));
        }
        first = false;
        if current == b'[' && pattern.get(at + 1) == Some(&b':') {
            let rest = &pattern[at + 2..];
            let close = rest.windows(2).position(|pair| pair == b":]")?;
            matched |= class(&rest[..close], byte);
            at += 2 + close + 2;
            continue;
        }
        let low = if current == b'\\' {
            at += 1;
            *pattern.get(at)?
        } else {
            current
        };
        at += 1;
        if pattern.get(at) == Some(&b'-')
            && pattern.get(at + 1).is_some_and(|&b| b != b']')
        {
            let mut high = pattern[at + 1];
            at += 2;
            if high == b'\\' {
                high = *pattern.get(at)?;
                at += 1;
            }
            matched |= low <= byte && byte <= high;
        } else {
            matched |= low == byte;
        }
    }
}

fn class(name: &[u8], byte: u8) -> bool {
    match name {
        b"alnum" => byte.is_ascii_alphanumeric(),
        b"alpha" => byte.is_ascii_alphabetic(),
        b"blank" => byte == b' ' || byte == b'\t',
        b"cntrl" => byte.is_ascii_control(),
        b"digit" => byte.is_ascii_digit(),
        b"graph" => byte.is_ascii_graphic(),
        b"lower" => byte.is_ascii_lowercase(),
        b"print" => byte.is_ascii_graphic() || byte == b' ',
        b"punct" => byte.is_ascii_punctuation(),
        b"space" => byte.is_ascii_whitespace() || byte == 0x0b,
        b"upper" => byte.is_ascii_uppercase(),
        b"xdigit" => byte.is_ascii_hexdigit(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_basics() {
        assert!(fnmatch(b"*.o", b"a/b.o"), "star crosses a slash");
        assert!(fnmatch(b"*", b".hidden"), "no special leading dot");
        assert!(fnmatch(b"a?c", b"abc"));
        assert!(!fnmatch(b"a?c", b"ac"));
        assert!(fnmatch(b"[a-c]x", b"bx"));
        assert!(fnmatch(b"[!a-c]x", b"dx"));
        assert!(!fnmatch(b"[!a-c]x", b"ax"));
        assert!(fnmatch(b"[]]", b"]"));
        assert!(fnmatch(b"[[:digit:]]*", b"7z"));
        assert!(fnmatch(b"\\*", b"*"));
        assert!(!fnmatch(b"\\*", b"x"));
        assert!(fnmatch(b"a*b*c", b"axxbyyc"));
        assert!(!fnmatch(b"a*b*c", b"axxbyy"));
        assert!(fnmatch(b"[x", b"[x"), "unterminated bracket is literal");
    }

    #[test]
    fn excludes_match_any_trailing_component_run() {
        let mut excludes = Excludes::default();
        excludes.add(b"node_modules");
        assert!(excludes.excludes(b"a/b/node_modules"));
        assert!(excludes.excludes(b"node_modules"));
        assert!(!excludes.excludes(b"a/node_modules/x"));
        let mut excludes = Excludes::default();
        excludes.add(b"b/*.log");
        assert!(excludes.excludes(b"./a/b/c.log"));
        assert!(!excludes.excludes(b"./a/bb/c.log"));
    }
}
