//! What a caller asks for: the root, how to measure it, and which entries
//! to list. The command line and the MCP tools both build a [`Query`].

use std::path::{Path, PathBuf};

use disktree_core::classify::{Category, Reclaim};
use disktree_core::exclude::Exclude;
use disktree_core::scan::ScanOptions;

/// Entries listed per section unless asked otherwise.
pub const DEFAULT_TOP: usize = 20;
/// The most entries a section lists; a longer list is no use to an agent.
pub const MAX_TOP: usize = 10_000;

/// How the tree is measured. Two queries with equal settings can share one
/// scan, which is what the MCP server's cache keys on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanSettings {
    /// Apparent length (`ls -l`) rather than allocated blocks (`du`).
    pub apparent_size: bool,
    /// Stay on the root's volume. On by default, as in the window.
    pub one_filesystem: bool,
    /// Include dotfiles and dot-directories. On by default.
    pub include_hidden: bool,
    /// Follow symlinks. Off by default.
    pub follow_links: bool,
    /// Patterns left out of the walk; see [`disktree_core::exclude`].
    pub exclude: Vec<String>,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            apparent_size: false,
            one_filesystem: true,
            include_hidden: true,
            follow_links: false,
            exclude: Vec::new(),
        }
    }
}

impl ScanSettings {
    /// The settings a scan was made with, as the window holds them.
    pub fn from_options(options: &ScanOptions) -> Self {
        Self {
            apparent_size: options.apparent_size,
            one_filesystem: options.one_filesystem,
            include_hidden: options.include_hidden,
            follow_links: options.follow_links,
            exclude: options.exclude.patterns().map(str::to_owned).collect(),
        }
    }

    /// The core scanner's options for these settings.
    pub fn options(&self) -> Result<ScanOptions, String> {
        Ok(ScanOptions {
            apparent_size: self.apparent_size,
            one_filesystem: self.one_filesystem,
            include_hidden: self.include_hidden,
            follow_links: self.follow_links,
            exclude: Exclude::new(&self.exclude)?,
            ..ScanOptions::default()
        })
    }

    pub const fn size_mode(&self) -> &'static str {
        if self.apparent_size {
            "apparent"
        } else {
            "on_disk"
        }
    }
}

/// Which parts a report carries. The totals, options and errors always
/// come; the rest is asked for, so a tool that wants one list does not pay
/// for the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sections {
    /// The volume, the three breakdowns and the reclaimable summary.
    pub overview: bool,
    /// The volume and the reclaimable summary alone.
    pub reclaim: bool,
    pub dirs: bool,
    pub files: bool,
}

impl Sections {
    pub const ALL: Self = Self {
        overview: true,
        reclaim: true,
        dirs: true,
        files: true,
    };
    pub const NONE: Self = Self {
        overview: false,
        reclaim: false,
        dirs: false,
        files: false,
    };
    pub const DIRS: Self = Self {
        dirs: true,
        ..Self::NONE
    };
    pub const FILES: Self = Self {
        files: true,
        ..Self::NONE
    };
    pub const RECLAIM: Self = Self {
        reclaim: true,
        ..Self::NONE
    };

    /// Whether the volume is measured.
    pub const fn volume(self) -> bool {
        self.overview || self.reclaim
    }

    /// Whether the reclaimable summary is made.
    pub const fn reclaimable(self) -> bool {
        self.overview || self.reclaim
    }

    /// `overview,dirs`: a comma list of `overview`, `reclaim`, `dirs`,
    /// `files` and `all`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut sections = Self::NONE;
        for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "all" => sections = Self::ALL,
                "overview" | "summary" => sections.overview = true,
                "reclaim" | "reclaimable" => sections.reclaim = true,
                "dirs" | "directories" => sections.dirs = true,
                "files" => sections.files = true,
                other => {
                    return Err(format!(
                        "unknown section {other:?}; try overview, reclaim, \
                         dirs, files or all"
                    ));
                }
            }
        }
        if sections == Self::NONE {
            return Err("name at least one section".to_owned());
        }
        Ok(sections)
    }
}

/// Before when an entry must have last been written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OlderThan {
    /// This many seconds before the report is made.
    Age(i64),
    /// Before this Unix time.
    Before(i64),
}

impl OlderThan {
    /// The cutoff in Unix seconds, for a report made at `now`.
    pub const fn cutoff(self, now: i64) -> i64 {
        match self {
            Self::Age(seconds) => now - seconds,
            Self::Before(time) => time,
        }
    }
}

/// Which reclaim reasons pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimFilter {
    /// Any reason at all.
    Any,
    One(Reclaim),
}

/// What an entry must be to be listed. Every set filter must pass.
#[derive(Clone, Debug, Default)]
pub struct Filters {
    /// At least this many bytes.
    pub min_size: Option<u64>,
    /// Files only: one of these extensions, lowercase, without the dot.
    pub extensions: Vec<String>,
    /// Last written before this: for a directory, nothing beneath it was
    /// written since.
    pub older_than: Option<OlderThan>,
    /// The name (or, with a `/`, the path) matches one of these patterns,
    /// in the exclude syntax.
    pub names: Vec<String>,
    /// Of this kind, as the window colours it.
    pub category: Option<Category>,
    /// Space the window marks as reclaimable, for this reason or any.
    pub reclaim: Option<ReclaimFilter>,
}

impl Filters {
    pub const fn is_empty(&self) -> bool {
        self.min_size.is_none()
            && self.extensions.is_empty()
            && self.older_than.is_none()
            && self.names.is_empty()
            && self.category.is_none()
            && self.reclaim.is_none()
    }

    /// A category or reclaim filter lists only the outermost matching
    /// directory: both are inherited, so every folder inside a cache is a
    /// cache too, and listing them all says nothing new.
    pub const fn outermost_dirs(&self) -> bool {
        self.category.is_some() || self.reclaim.is_some()
    }
}

/// Everything a report needs to be made.
#[derive(Clone, Debug)]
pub struct Query {
    pub root: PathBuf,
    pub settings: ScanSettings,
    /// Entries per section.
    pub top: usize,
    /// List entries at most this many levels below the root. Sizes still
    /// count everything beneath.
    pub depth: Option<usize>,
    pub sections: Sections,
    pub filters: Filters,
}

impl Query {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            settings: ScanSettings::default(),
            top: DEFAULT_TOP,
            depth: None,
            sections: Sections::ALL,
            filters: Filters::default(),
        }
    }
}

/// `~` and `~/…` against the home directory; anything else as given.
pub fn expand_home(text: &str) -> PathBuf {
    let home = std::env::home_dir();
    match (text, home) {
        ("~", Some(home)) => home,
        (text, Some(home)) if text.starts_with("~/") => home.join(&text[2..]),
        _ => PathBuf::from(text),
    }
}

/// The root as the scanner should see it: canonical, and a directory.
pub fn resolve_root(path: &Path) -> Result<PathBuf, String> {
    let root = dunce::canonicalize(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    Ok(root)
}

/// Entries per section: 1 to [`MAX_TOP`].
pub fn check_top(top: u64) -> Result<usize, String> {
    usize::try_from(top)
        .ok()
        .filter(|top| (1..=MAX_TOP).contains(top))
        .ok_or_else(|| format!("top must be 1 to {MAX_TOP}"))
}

/// Levels below the root: at least 1.
pub fn check_depth(depth: u64) -> Result<usize, String> {
    usize::try_from(depth)
        .ok()
        .filter(|depth| *depth >= 1)
        .ok_or_else(|| "depth must be 1 or more".to_owned())
}

/// A size: `1500`, `500K`, `1.5G`, `2GB`, `2GiB`, `10 MB`. Units are
/// binary whichever way they are spelled, matching the `GiB` reports print:
/// `1GB` is 1024³ bytes.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let invalid = || {
        format!("cannot read size {text:?}; try 1500, 500K, 1.5G, 2GB or 2GiB")
    };
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let value: f64 = number.parse().map_err(|_| invalid())?;
    let power = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 1,
        "m" | "mb" | "mib" => 2,
        "g" | "gb" | "gib" => 3,
        "t" | "tb" | "tib" => 4,
        "p" | "pb" | "pib" => 5,
        _ => return Err(invalid()),
    };
    let bytes = value * 1024_f64.powi(power);
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(invalid());
    }
    Ok(bytes.round() as u64)
}

/// An age or a date: `30d`, `12h`, `2w`, `6mo`, `1y`, `90min`, `45s`, or
/// `2025-01-31` (midnight UTC). A month is 30 days and a year 365.
pub fn parse_older_than(text: &str) -> Result<OlderThan, String> {
    let trimmed = text.trim();
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let time = date
            .and_hms_opt(0, 0, 0)
            .map(|time| time.and_utc().timestamp())
            .ok_or_else(|| format!("cannot read date {text:?}"))?;
        return Ok(OlderThan::Before(time));
    }
    let invalid = || {
        format!(
            "cannot read age {text:?}; try 30d, 12h, 2w, 6mo, 1y, 90min or \
             a date like 2025-01-31"
        )
    };
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (number, unit) = trimmed.split_at(split);
    let count: i64 = number.parse().map_err(|_| invalid())?;
    let unit_seconds = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "week" | "weeks" => 7 * 86_400,
        "mo" | "month" | "months" => 30 * 86_400,
        "y" | "yr" | "year" | "years" => 365 * 86_400,
        _ => return Err(invalid()),
    };
    count
        .checked_mul(unit_seconds)
        .map(OlderThan::Age)
        .ok_or_else(invalid)
}

/// `iso`, `.ISO` and `tar.gz` alike: lowercase, no leading dot.
pub fn normalize_extension(text: &str) -> Option<String> {
    let text = text.trim().trim_start_matches('.').to_ascii_lowercase();
    (!text.is_empty()).then_some(text)
}

/// The slug a category is written as in JSON.
pub const fn category_slug(category: Category) -> &'static str {
    match category {
        Category::Code => "code",
        Category::AgentScratch => "agent-scratch",
        Category::Toolchain => "toolchain",
        Category::Synced => "synced",
        Category::Git => "git",
        Category::Media => "media",
        Category::Documents => "documents",
        Category::Cache => "cache",
        Category::Other => "other",
    }
}

/// The slug a reclaim reason is written as in JSON.
pub const fn reclaim_slug(reclaim: Reclaim) -> &'static str {
    match reclaim {
        Reclaim::Regenerable => "regenerable",
        Reclaim::SyncHistory => "sync-history",
        Reclaim::PackageStore => "package-store",
        Reclaim::BuildOutput => "build-output",
        Reclaim::Reinstallable => "reinstallable",
        Reclaim::SandboxLayers => "sandbox-layers",
        Reclaim::Snapshots => "snapshots",
        Reclaim::Trash => "trash",
        Reclaim::Temporary => "temporary",
    }
}

const CATEGORIES: [Category; 9] = [
    Category::Code,
    Category::AgentScratch,
    Category::Toolchain,
    Category::Synced,
    Category::Git,
    Category::Media,
    Category::Documents,
    Category::Cache,
    Category::Other,
];

const RECLAIMS: [Reclaim; 9] = [
    Reclaim::Regenerable,
    Reclaim::SyncHistory,
    Reclaim::PackageStore,
    Reclaim::BuildOutput,
    Reclaim::Reinstallable,
    Reclaim::SandboxLayers,
    Reclaim::Snapshots,
    Reclaim::Trash,
    Reclaim::Temporary,
];

/// Every category slug, for help text and the MCP schema.
pub fn category_slugs() -> Vec<&'static str> {
    CATEGORIES.into_iter().map(category_slug).collect()
}

/// Every reclaim slug, plus `any`.
pub fn reclaim_slugs() -> Vec<&'static str> {
    std::iter::once("any")
        .chain(RECLAIMS.into_iter().map(reclaim_slug))
        .collect()
}

pub fn parse_category(text: &str) -> Result<Category, String> {
    let wanted = text.trim().to_ascii_lowercase().replace('_', "-");
    CATEGORIES
        .into_iter()
        .find(|category| category_slug(*category) == wanted)
        .ok_or_else(|| {
            format!(
                "unknown category {text:?}; try {}",
                category_slugs().join(", ")
            )
        })
}

pub fn parse_reclaim(text: &str) -> Result<ReclaimFilter, String> {
    let wanted = text.trim().to_ascii_lowercase().replace('_', "-");
    if wanted == "any" {
        return Ok(ReclaimFilter::Any);
    }
    RECLAIMS
        .into_iter()
        .find(|reclaim| reclaim_slug(*reclaim) == wanted)
        .map(ReclaimFilter::One)
        .ok_or_else(|| {
            format!(
                "unknown reclaim reason {text:?}; try {}",
                reclaim_slugs().join(", ")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn sizes_are_binary_however_they_are_spelled() {
        assert_eq!(parse_size("1500"), Ok(1500));
        assert_eq!(parse_size("1G"), Ok(GIB));
        assert_eq!(parse_size("1GB"), Ok(GIB));
        assert_eq!(parse_size("1 GiB"), Ok(GIB));
        assert_eq!(parse_size("1.5g"), Ok(GIB * 3 / 2));
        assert_eq!(parse_size("500K"), Ok(500 * 1024));
        assert_eq!(parse_size("2tb"), Ok(2 * 1024 * GIB));
        for bad in ["", "G", "1XB", "-1G", "1..5G", "99999999999P"] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ages_and_dates_are_read() {
        assert_eq!(parse_older_than("30d"), Ok(OlderThan::Age(30 * 86_400)));
        assert_eq!(parse_older_than("12h"), Ok(OlderThan::Age(12 * 3_600)));
        assert_eq!(parse_older_than("6mo"), Ok(OlderThan::Age(180 * 86_400)));
        assert_eq!(parse_older_than("1y"), Ok(OlderThan::Age(365 * 86_400)));
        assert_eq!(parse_older_than("90min"), Ok(OlderThan::Age(5_400)));
        assert_eq!(
            parse_older_than("2025-01-31"),
            Ok(OlderThan::Before(1_738_281_600))
        );
        // `m` would be minutes to some and months to others.
        for bad in ["", "d", "30", "30m", "30x", "2025-13-01"] {
            assert!(parse_older_than(bad).is_err(), "{bad:?}");
        }
        assert_eq!(OlderThan::Age(10).cutoff(100), 90);
        assert_eq!(OlderThan::Before(5).cutoff(100), 5);
    }

    #[test]
    fn extensions_categories_and_reasons_are_normalized() {
        assert_eq!(normalize_extension(".ISO"), Some("iso".to_owned()));
        assert_eq!(normalize_extension("tar.gz"), Some("tar.gz".to_owned()));
        assert_eq!(normalize_extension(" . "), None);
        assert_eq!(parse_category("Cache"), Ok(Category::Cache));
        assert_eq!(parse_category("agent_scratch"), Ok(Category::AgentScratch));
        assert!(parse_category("caches").is_err());
        assert_eq!(parse_reclaim("any"), Ok(ReclaimFilter::Any));
        assert_eq!(
            parse_reclaim("build-output"),
            Ok(ReclaimFilter::One(Reclaim::BuildOutput))
        );
        assert!(parse_reclaim("junk").is_err());
        for slug in category_slugs() {
            assert_eq!(category_slug(parse_category(slug).expect(slug)), slug);
        }
    }

    #[test]
    fn top_and_depth_are_bounded() {
        assert_eq!(check_top(5), Ok(5));
        assert!(check_top(0).is_err());
        assert!(check_top(MAX_TOP as u64 + 1).is_err());
        assert_eq!(check_depth(1), Ok(1));
        assert!(check_depth(0).is_err());
    }

    #[test]
    fn home_is_expanded_only_at_the_start() {
        if let Some(home) = std::env::home_dir() {
            assert_eq!(expand_home("~"), home);
            assert_eq!(expand_home("~/x"), home.join("x"));
        }
        assert_eq!(expand_home("/a/~/b"), PathBuf::from("/a/~/b"));
        assert_eq!(expand_home("~other"), PathBuf::from("~other"));
    }
}
