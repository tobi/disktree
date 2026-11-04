//! What Settings › Safety saved: the removal limit and what Review asks
//! before going past it. MCP cannot change either, and an agent is held
//! to a limit even when the window's is off.

use std::path::Path;

use disktree_core::removal_limit::Limit;

const FILE: &str = "removal-limit-percent";
const CONFIRM_FILE: &str = "over-limit-confirm";
/// Saved in place of a percent when the person turned the limit off.
const OFF: &str = "off";
/// What Settings saved for no limit before Off existed.
const LEGACY_OFF: &str = "100";

/// What Review asks before a removal past the limit, after Mark anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Confirm {
    /// Type "delete 20% of my disk": reading it is the point.
    #[default]
    Phrase,
    /// Tick a box that says the same.
    Tick,
    /// Nothing more: Mark anyway was the confirmation.
    Nothing,
}

impl Confirm {
    pub const ALL: [Self; 3] = [Self::Phrase, Self::Tick, Self::Nothing];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Phrase => "Type a phrase",
            Self::Tick => "Tick a box",
            Self::Nothing => "No second step",
        }
    }

    const fn word(self) -> &'static str {
        match self {
            Self::Phrase => "phrase",
            Self::Tick => "tick",
            Self::Nothing => "none",
        }
    }
}

/// The window's removal policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// `None` when the person turned the limit off for the window.
    pub limit: Option<Limit>,
    pub confirm: Confirm,
}

/// The default limit, on: off is only ever the person's own choice.
impl Default for Policy {
    fn default() -> Self {
        Self {
            limit: Some(Limit::default()),
            confirm: Confirm::default(),
        }
    }
}

impl Policy {
    /// The limit an agent is held to: the window's, or the default when
    /// the window's is off. An agent can never confirm past a limit for
    /// the person, so turning it off for the window must not lift it.
    pub fn agent_limit(self) -> Limit {
        self.limit.unwrap_or_default()
    }
}

/// The limit an agent's removal is held to; see [`Policy::agent_limit`].
pub fn load(dir: Option<&Path>) -> Result<Limit, String> {
    load_policy(dir).map(Policy::agent_limit)
}

/// Both settings. A file that cannot be read or makes no sense refuses
/// removal rather than falling back to something laxer.
pub fn load_policy(dir: Option<&Path>) -> Result<Policy, String> {
    let Some(dir) = dir else {
        return Ok(Policy::default());
    };
    let limit = match read(&dir.join(FILE))? {
        None => Some(Limit::default()),
        // 100% was the last choice before Off existed and meant the same
        // for the window; read as Off, an agent keeps the default.
        Some(text) if text == OFF || text == LEGACY_OFF => None,
        Some(text) => Some(
            text.parse::<u8>()
                .map_err(|_| "Invalid removal limit in Settings > Safety")
                .and_then(|percent| {
                    Limit::new(percent).map_err(
                        |_| "Invalid removal limit in Settings > Safety",
                    )
                })?,
        ),
    };
    let confirm = match read(&dir.join(CONFIRM_FILE))? {
        None => Confirm::default(),
        Some(text) => Confirm::ALL
            .into_iter()
            .find(|confirm| confirm.word() == text)
            .ok_or("Invalid over-limit confirmation in Settings > Safety")?,
    };
    Ok(Policy { limit, confirm })
}

fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Cannot read Settings > Safety: {error}")),
    }
}

/// Set once Settings › Safety saves, so the main window reads the policy
/// again rather than judging marks by the one it read at start.
pub struct Saved;
impl gpui_kit::Global for Saved {}

#[cfg(test)]
pub fn save(dir: Option<&Path>, limit: Limit) -> Result<(), String> {
    save_limit(dir, Some(limit))
}

/// Save the limit, or `None` to turn it off for the window.
pub fn save_limit(
    dir: Option<&Path>,
    limit: Option<Limit>,
) -> Result<(), String> {
    let text =
        limit.map_or_else(|| OFF.to_owned(), |l| l.percent().to_string());
    write(dir, FILE, &text)
}

pub fn save_confirm(
    dir: Option<&Path>,
    confirm: Confirm,
) -> Result<(), String> {
    write(dir, CONFIRM_FILE, confirm.word())
}

/// Write beside, then rename, so a reader never sees half a setting.
fn write(dir: Option<&Path>, file: &str, text: &str) -> Result<(), String> {
    let dir = dir.ok_or("No settings directory is available")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let pending = dir.join(format!("{file}.new"));
    std::fs::write(&pending, format!("{text}\n"))
        .map_err(|error| error.to_string())?;
    std::fs::rename(pending, dir.join(file)).map_err(|error| error.to_string())
}

/// The share of a volume `bytes` is, in whole percent rounded up, so
/// "20%" never undersells what goes. At most 100.
pub fn share_percent(bytes: u64, capacity: u64) -> u8 {
    if capacity == 0 {
        return 100;
    }
    let percent = (u128::from(bytes) * 100).div_ceil(u128::from(capacity));
    u8::try_from(percent.min(100)).unwrap_or(100)
}

/// What Review asks the person to type for a removal of `percents` of
/// disks in `mode`, one share per disk past the limit. The verb is the
/// mode's, so the phrase is read for how much, how, and how many disks:
/// "delete 20% of my disk", "trash 20% and 19% of my disks".
pub fn phrase(
    percents: &[u8],
    mode: disktree_core::removal::RemovalMode,
) -> String {
    let verb = match mode {
        disktree_core::removal::RemovalMode::Permanent => "delete",
        disktree_core::removal::RemovalMode::Trash => "trash",
        disktree_core::removal::RemovalMode::OwnerCleanup => "clean",
    };
    let disks = if percents.len() > 1 {
        "my disks"
    } else {
        "my disk"
    };
    let shares = series(percents.iter().map(|percent| format!("{percent}%")));
    format!("{verb} {shares} of {disks}")
}

/// `items` as a reader lists them: "a", "a and b", "a, b and c".
pub fn series(items: impl IntoIterator<Item = String>) -> String {
    let mut items: Vec<String> = items.into_iter().collect();
    let Some(last) = items.pop() else {
        return String::new();
    };
    if items.is_empty() {
        last
    } else {
        format!("{} and {last}", items.join(", "))
    }
}

/// Whether `typed` says `phrase`: case and the spaces around and between
/// words are forgiven, nothing else.
pub fn matches(typed: &str, phrase: &str) -> bool {
    let words = |text: &str| {
        text.split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    words(typed) == words(phrase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::removal::RemovalMode;

    #[test]
    fn defaults_persist_and_invalid_configuration_refuses_removal() {
        let temp = tempfile::tempdir().expect("settings");
        let dir = Some(temp.path());
        assert_eq!(load(dir).expect("default").percent(), 5);
        assert_eq!(load_policy(dir).expect("default"), Policy::default());
        save(dir, Limit::new(10).expect("10%")).expect("save");
        assert_eq!(load(dir).expect("saved").percent(), 10);
        std::fs::write(temp.path().join(FILE), "0").expect("invalid setting");
        assert!(load(dir).is_err());
        std::fs::write(temp.path().join(CONFIRM_FILE), "maybe")
            .expect("invalid setting");
        save(dir, Limit::default()).expect("save");
        assert!(load_policy(dir).is_err(), "an unknown confirmation refuses");
    }

    #[test]
    fn off_lifts_the_windows_limit_but_never_an_agents() {
        let temp = tempfile::tempdir().expect("settings");
        let dir = Some(temp.path());
        save_limit(dir, None).expect("off");
        save_confirm(dir, Confirm::Tick).expect("tick");
        let policy = load_policy(dir).expect("saved");
        assert_eq!(policy.limit, None);
        assert_eq!(policy.confirm, Confirm::Tick);
        assert_eq!(load(dir).expect("agent").percent(), 5);
        std::fs::write(temp.path().join(FILE), "100\n").expect("legacy");
        assert_eq!(load_policy(dir).expect("legacy").limit, None);
        assert_eq!(load(dir).expect("agent").percent(), 5);
    }

    #[test]
    fn the_share_rounds_up_and_the_phrase_says_it() {
        assert_eq!(share_percent(181, 926), 20);
        assert_eq!(share_percent(200, 1000), 20, "exact stays exact");
        assert_eq!(share_percent(201, 1000), 21);
        assert_eq!(share_percent(5, 0), 100, "no capacity is the worst case");
        assert_eq!(share_percent(u64::MAX, 1), 100);
        let typed = phrase(&[20], RemovalMode::Permanent);
        assert_eq!(typed, "delete 20% of my disk");
        assert_eq!(
            phrase(&[20, 19], RemovalMode::Trash),
            "trash 20% and 19% of my disks"
        );
        assert_eq!(
            phrase(&[20, 19, 7], RemovalMode::Permanent),
            "delete 20%, 19% and 7% of my disks"
        );
        assert_eq!(series(Vec::new()), "");
        assert!(matches("  Delete 20%  of my DISK ", &typed));
        assert!(!matches("delete 2% of my disk", &typed));
        assert!(!matches("trash 20% of my disk", &typed));
        assert!(!matches("", &typed));
        assert_eq!(
            phrase(&[20], RemovalMode::OwnerCleanup),
            "clean 20% of my disk"
        );
    }
}
