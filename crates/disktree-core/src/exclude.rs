//! Names and paths a scan leaves out, by pattern.
//!
//! A pattern without a `/` is matched against an entry's name, so
//! `node_modules` or `*.iso` leaves those out wherever they are. A pattern
//! with one is matched against the entry's whole path, so
//! `/Users/me/Library/**/Caches` names one place. `*` matches any run of
//! characters within one path component, `**` any run across them, and `?`
//! one character. Matching is case-sensitive. An excluded directory is not
//! entered, so its contents cost nothing and count for nothing.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// A compiled set of exclude patterns. Cheap to clone: scans copy their
/// options.
#[derive(Clone, Default)]
pub struct Exclude {
    patterns: Arc<[Pattern]>,
    /// Whether any pattern needs the full path, which is otherwise never
    /// built: a scan meets millions of entries, nearly all of them files.
    any_path: bool,
}

#[derive(Debug)]
struct Pattern {
    text: Box<str>,
    tokens: Box<[Token]>,
    /// Matched against the full path rather than the name.
    path: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Literal(char),
    /// `?`: one character, not a separator.
    One,
    /// `*`: any run within a component.
    Star,
    /// `**`: any run, separators included.
    Globstar,
}

impl Exclude {
    /// Compile `patterns`; blank ones are ignored. Fails only on a pattern
    /// too long to match, over 127 characters.
    pub fn new<I, S>(patterns: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut compiled = Vec::new();
        for text in patterns {
            compiled.extend(compile(text.as_ref())?);
        }
        let patterns: Arc<[Pattern]> = compiled.into();
        let any_path = patterns.iter().any(|pattern| pattern.path);
        Ok(Self { patterns, any_path })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The patterns as given, for reporting what a scan left out.
    pub fn patterns(&self) -> impl Iterator<Item = &str> {
        self.patterns.iter().map(|pattern| &*pattern.text)
    }

    /// Whether the entry called `name` is left out. `path` builds its full
    /// path and is only called when some pattern needs it.
    pub fn matches(
        &self,
        name: &str,
        path: impl FnOnce() -> std::path::PathBuf,
    ) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        if self
            .patterns
            .iter()
            .any(|pattern| !pattern.path && glob(&pattern.tokens, name))
        {
            return true;
        }
        if !self.any_path {
            return false;
        }
        let path = path();
        let text = normalized(&path);
        self.patterns
            .iter()
            .any(|pattern| pattern.path && glob(&pattern.tokens, &text))
    }

    /// [`Self::matches`] for a path already in hand.
    pub fn matches_path(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        self.matches(&name, || path.to_path_buf())
    }
}

/// On the wire, the patterns as given: a list of strings.
impl serde::Serialize for Exclude {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.patterns())
    }
}

/// Compiled again on arrival, so a pattern the receiver cannot match is an
/// error rather than a silently wider scan.
impl<'de> serde::Deserialize<'de> for Exclude {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        let patterns = Vec::<String>::deserialize(deserializer)?;
        Self::new(patterns).map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for Exclude {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.patterns()).finish()
    }
}

/// The path as text with `/` separators, which is what patterns are
/// written in on every platform.
fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// Positions a pattern can have, one bit each, plus the end.
const MAX_TOKENS: usize = 127;

fn compile(text: &str) -> Result<Option<Pattern>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let source = if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.to_owned()
    };
    // A trailing separator would never match: paths are compared without
    // one.
    let source = source.trim_end_matches('/');
    let source = if source.is_empty() { "/" } else { source };
    let mut tokens = Vec::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        let token = match c {
            '*' if chars.peek() == Some(&'*') => {
                while chars.peek() == Some(&'*') {
                    chars.next();
                }
                Token::Globstar
            }
            '*' => Token::Star,
            '?' => Token::One,
            other => Token::Literal(other),
        };
        // Adjacent stars are one star, and a star beside a globstar is the
        // globstar: the same pattern in fewer positions.
        match (tokens.last_mut(), token) {
            (Some(last @ Token::Star), Token::Globstar) => {
                *last = Token::Globstar;
            }
            (Some(Token::Star | Token::Globstar), Token::Star)
            | (Some(Token::Globstar), Token::Globstar) => {}
            _ => tokens.push(token),
        }
    }
    if tokens.len() > MAX_TOKENS {
        return Err(format!(
            "exclude pattern {text:?} is longer than {MAX_TOKENS} characters"
        ));
    }
    Ok(Some(Pattern {
        text: text.into(),
        path: source.contains('/'),
        tokens: tokens.into(),
    }))
}

/// Whether `tokens` match all of `text`: the pattern run as an automaton
/// over a bit set of positions, so stars never backtrack and a hostile
/// pattern costs no more than a plain one.
fn glob(tokens: &[Token], text: &str) -> bool {
    let end = tokens.len();
    let mut states = closure(tokens, 1);
    for c in text.chars() {
        let mut next = 0_u128;
        for (index, token) in tokens.iter().enumerate() {
            if states & (1 << index) == 0 {
                continue;
            }
            match *token {
                Token::Literal(want) if want == c => next |= 1 << (index + 1),
                Token::One if c != '/' => next |= 1 << (index + 1),
                Token::Star if c != '/' => next |= 1 << index,
                Token::Globstar => next |= 1 << index,
                _ => {}
            }
        }
        if next == 0 {
            return false;
        }
        states = closure(tokens, next);
    }
    states & (1 << end) != 0
}

/// `states` plus every position reachable by letting a star match nothing.
fn closure(tokens: &[Token], mut states: u128) -> u128 {
    for (index, token) in tokens.iter().enumerate() {
        if states & (1 << index) != 0
            && matches!(token, Token::Star | Token::Globstar)
        {
            states |= 1 << (index + 1);
        }
    }
    states
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn excluded(patterns: &[&str], path: &str) -> bool {
        Exclude::new(patterns)
            .expect("compiles")
            .matches_path(Path::new(path))
    }

    #[test]
    fn a_bare_pattern_matches_the_name_anywhere() {
        assert!(excluded(&["node_modules"], "/a/b/node_modules"));
        assert!(!excluded(&["node_modules"], "/a/node_modules_old"));
        assert!(excluded(&["*.iso"], "/x/Ubuntu.iso"));
        assert!(!excluded(&["*.iso"], "/x/iso"));
        assert!(excluded(&["cache?"], "/x/cache1"));
        assert!(!excluded(&["cache?"], "/x/cache"));
        assert!(excluded(&[".*"], "/x/.git"));
    }

    #[test]
    fn a_pattern_with_a_separator_matches_the_whole_path() {
        assert!(excluded(&["/Users/*/Library"], "/Users/me/Library"));
        // `*` stays within one component.
        assert!(!excluded(&["/Users/*/Library"], "/Users/me/x/Library"));
        assert!(excluded(&["/Users/**/Caches"], "/Users/me/Library/Caches"));
        assert!(excluded(&["**/target/debug"], "/src/app/target/debug"));
        assert!(!excluded(&["**/target/debug"], "/src/app/target/release"));
        // A trailing separator is the same pattern.
        assert!(excluded(&["/data/"], "/data"));
    }

    #[test]
    fn blank_patterns_are_ignored_and_the_path_is_built_only_on_demand() {
        let exclude = Exclude::new(["", "  ", "*.tmp"]).expect("compiles");
        assert_eq!(exclude.patterns().collect::<Vec<_>>(), ["*.tmp"]);
        assert!(exclude.matches("a.tmp", || unreachable!("name is enough")));
        assert!(!exclude.matches("a.txt", || unreachable!("no path pattern")));
        assert!(Exclude::new::<_, &str>([]).expect("empty").is_empty());
        assert!(Exclude::new(["x".repeat(200)]).is_err());
        assert!(!Exclude::default().matches("x", PathBuf::new));
    }

    #[test]
    fn patterns_travel_as_a_list_of_strings() {
        let exclude =
            Exclude::new(["node_modules", "/srv/**/cache"]).expect("compiles");
        let wire = serde_json::to_string(&exclude).expect("serializes");
        assert_eq!(wire, r#"["node_modules","/srv/**/cache"]"#);
        let back: Exclude = serde_json::from_str(&wire).expect("parses");
        assert!(back.matches_path(Path::new("/a/node_modules")));
        assert!(back.matches_path(Path::new("/srv/x/cache")));
        let long = serde_json::to_string(&["x".repeat(200)]).expect("json");
        assert!(serde_json::from_str::<Exclude>(&long).is_err());
    }

    #[test]
    fn many_stars_do_not_blow_up() {
        let exclude = Exclude::new(["*a*a*a*a*a*a*a*b"]).expect("compiles");
        assert!(!exclude.matches(&"a".repeat(10_000), PathBuf::new));
        assert!(
            exclude.matches(&format!("{}b", "a".repeat(10_000)), PathBuf::new)
        );
    }
}
