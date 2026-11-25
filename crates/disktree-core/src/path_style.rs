//! How paths on the scanned machine are joined, split and compared.
//!
//! A local scan follows this machine's rules, which is `std::path`. An SSH
//! host is always Unix: the remote command, the embedded scanners and the
//! delete script are POSIX shell and Linux tools. So remote paths follow
//! POSIX rules on every client. On a Windows client `std::path` would join
//! them with `\`, naming a different file on the host, and split a Unix name
//! such as `a\b` or `c:d` into parts the host never had.
//!
//! Remote paths only ever arrive as JSON strings, so the POSIX side works on
//! `str` and treats a path that is not UTF-8 as matching nothing.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The path rules of the machine a tree was scanned on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathStyle {
    /// This machine's own rules.
    #[default]
    Native,
    /// `/`-separated, as on the Unix host an SSH session scans.
    Posix,
}

impl PathStyle {
    /// The style for a local scan, or for a scan of an SSH host.
    #[must_use]
    pub const fn of(remote: bool) -> Self {
        if remote { Self::Posix } else { Self::Native }
    }

    /// The separator shown between names.
    #[must_use]
    pub const fn separator(self) -> char {
        match self {
            Self::Native => std::path::MAIN_SEPARATOR,
            Self::Posix => '/',
        }
    }

    /// Whether `path` names a directory from the machine's root.
    #[must_use]
    pub fn is_absolute(self, path: &Path) -> bool {
        match self {
            Self::Native => path.is_absolute(),
            Self::Posix => path.to_str().is_some_and(|s| s.starts_with('/')),
        }
    }

    /// `base` with one more name below it.
    #[must_use]
    pub fn join(self, base: &Path, name: &str) -> PathBuf {
        let mut path = base.to_path_buf();
        self.push(&mut path, name);
        path
    }

    /// Append one name to `path`.
    pub fn push(self, path: &mut PathBuf, name: &str) {
        match self {
            Self::Native => path.push(name),
            Self::Posix => {
                let mut joined = path.to_string_lossy().into_owned();
                if !joined.is_empty() && !joined.ends_with('/') {
                    joined.push('/');
                }
                joined.push_str(name);
                *path = PathBuf::from(joined);
            }
        }
    }

    /// The last name in `path`, or `None` for the root.
    #[must_use]
    pub fn file_name(self, path: &Path) -> Option<&OsStr> {
        match self {
            Self::Native => path.file_name(),
            Self::Posix => {
                let trimmed = path.to_str()?.trim_end_matches('/');
                let name = trimmed.rsplit('/').next()?;
                (!name.is_empty() && name != "..").then(|| OsStr::new(name))
            }
        }
    }

    /// `path` without its last name, or `None` for the root.
    #[must_use]
    pub fn parent(self, path: &Path) -> Option<&Path> {
        match self {
            Self::Native => path.parent(),
            Self::Posix => {
                let trimmed = path.to_str()?.trim_end_matches('/');
                if trimmed.is_empty() {
                    return None;
                }
                Some(Path::new(match trimmed.rfind('/') {
                    None => "",
                    Some(at) => match trimmed[..at].trim_end_matches('/') {
                        "" => "/",
                        head => head,
                    },
                }))
            }
        }
    }

    /// `path`, its parent, its parent's parent, and so on to the root.
    pub fn ancestors(self, path: &Path) -> impl Iterator<Item = &Path> {
        std::iter::successors(Some(path), move |path| self.parent(path))
    }

    /// The names from `base` down to `path`, or `None` when `path` is not
    /// `base` or below it. Names are compared whole, so a name containing a
    /// separator of some other platform cannot be mistaken for two.
    #[must_use]
    pub fn relative<'a>(
        self,
        base: &Path,
        path: &'a Path,
    ) -> Option<Vec<Cow<'a, str>>> {
        match self {
            Self::Native => {
                let rest = path.strip_prefix(base).ok()?;
                Some(
                    rest.components()
                        .map(|part| part.as_os_str().to_string_lossy())
                        .collect(),
                )
            }
            Self::Posix => posix_relative(base.to_str()?, path.to_str()?)
                .map(|names| names.map(Cow::Borrowed).collect()),
        }
    }

    /// Every name in `path` below its root, in order.
    #[must_use]
    pub fn names(self, path: &Path) -> Vec<Cow<'_, str>> {
        match self {
            Self::Native => path
                .components()
                .filter(|part| matches!(part, std::path::Component::Normal(_)))
                .map(|part| part.as_os_str().to_string_lossy())
                .collect(),
            Self::Posix => path.to_str().map_or_else(Vec::new, |path| {
                path.split('/')
                    .filter(|name| !name.is_empty() && *name != ".")
                    .map(Cow::Borrowed)
                    .collect()
            }),
        }
    }

    /// Whether `path` is strictly below `base`.
    #[must_use]
    pub fn is_below(self, path: &Path, base: &Path) -> bool {
        match self {
            Self::Native => path != base && path.starts_with(base),
            Self::Posix => match (base.to_str(), path.to_str()) {
                (Some(base), Some(path)) => posix_relative(base, path)
                    .is_some_and(|mut names| names.next().is_some()),
                _ => false,
            },
        }
    }
}

fn posix_relative<'a>(
    base: &str,
    path: &'a str,
) -> Option<impl Iterator<Item = &'a str>> {
    let rest = path.strip_prefix(base.trim_end_matches('/'))?;
    // `/srv` must not claim `/srvx`; an empty base is a relative start.
    if !base.is_empty() && !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(
        rest.split('/')
            .filter(|name| !name.is_empty() && *name != "."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(style: PathStyle, base: &str, path: &str) -> Option<Vec<String>> {
        style
            .relative(Path::new(base), Path::new(path))
            .map(|names| names.into_iter().map(Cow::into_owned).collect())
    }

    /// The host's paths are joined with `/` whatever this machine uses.
    #[test]
    fn posix_paths_join_with_a_slash_on_every_client() {
        let posix = PathStyle::Posix;
        let joined = posix.join(Path::new("/home/x"), "child");
        assert_eq!(joined.to_str(), Some("/home/x/child"));
        let joined = posix.join(Path::new("/"), "srv");
        assert_eq!(joined.to_str(), Some("/srv"));
        let joined = posix.join(&joined, r"back\slash");
        assert_eq!(joined.to_str(), Some(r"/srv/back\slash"));
        assert_eq!(posix.join(Path::new(""), "a").to_str(), Some("a"));
    }

    /// Names a Windows client would split on stay whole on a Unix host.
    #[test]
    fn posix_names_keep_backslashes_and_colons() {
        let posix = PathStyle::Posix;
        let path = Path::new(r"/srv/a\b/c:d");
        assert_eq!(posix.file_name(path), Some(OsStr::new("c:d")));
        assert_eq!(posix.parent(path), Some(Path::new(r"/srv/a\b")));
        assert_eq!(
            names(posix, "/srv", r"/srv/a\b/c:d"),
            Some(vec![r"a\b".into(), "c:d".into()])
        );
        assert!(posix.is_below(path, Path::new(r"/srv/a\b")));
        assert_eq!(posix.names(path), ["srv", r"a\b", "c:d"]);
        assert!(!posix.is_below(path, Path::new("/srv/a")));
    }

    #[test]
    fn posix_parents_end_at_the_root() {
        let posix = PathStyle::Posix;
        let chain: Vec<_> = posix
            .ancestors(Path::new("/home/x/"))
            .map(|path| path.to_str().unwrap())
            .collect();
        assert_eq!(chain, ["/home/x/", "/home", "/"]);
        assert_eq!(posix.parent(Path::new("/")), None);
        assert_eq!(posix.file_name(Path::new("/")), None);
        assert_eq!(posix.parent(Path::new("name")), Some(Path::new("")));
    }

    #[test]
    fn posix_relative_names_respect_name_boundaries() {
        let posix = PathStyle::Posix;
        assert_eq!(names(posix, "/srv", "/srv"), Some(vec![]));
        assert_eq!(
            names(posix, "/", "/srv/x"),
            Some(vec!["srv".into(), "x".into()])
        );
        assert_eq!(names(posix, "/srv/", "/srv/x"), Some(vec!["x".into()]));
        assert_eq!(names(posix, "/srv", "/srvx"), None);
        assert_eq!(names(posix, "/srv", "/var"), None);
        assert!(!posix.is_below(Path::new("/srv"), Path::new("/srv")));
        assert!(posix.is_below(Path::new("/srv"), Path::new("/")));
    }

    /// Only `/`-rooted paths are absolute on a Unix host; `C:\x` and `\x`
    /// name nothing there.
    #[test]
    fn posix_absolute_means_slash_rooted() {
        let posix = PathStyle::Posix;
        assert!(posix.is_absolute(Path::new("/")));
        assert!(posix.is_absolute(Path::new("/home/someone")));
        assert!(!posix.is_absolute(Path::new("home/someone")));
        assert!(!posix.is_absolute(Path::new("")));
        assert!(!posix.is_absolute(Path::new(r"\home\someone")));
        assert!(!posix.is_absolute(Path::new(r"C:\Users")));
        assert_eq!(posix.separator(), '/');
    }

    /// Native is `std::path`, so local scans behave exactly as before.
    #[test]
    fn native_follows_this_machine() {
        let native = PathStyle::Native;
        let base = std::env::temp_dir();
        let joined = native.join(&base, "child");
        assert_eq!(joined, base.join("child"));
        assert_eq!(native.parent(&joined), Some(base.as_path()));
        assert_eq!(native.file_name(&joined), Some(OsStr::new("child")));
        assert!(native.is_below(&joined, &base));
        assert!(!native.is_below(&base, &base));
        assert_eq!(native.separator(), std::path::MAIN_SEPARATOR);
    }
}
