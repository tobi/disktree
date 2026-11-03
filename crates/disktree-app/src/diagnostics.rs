//! Diagnostics: what this copy runs with, as Settings › Diagnostics shows
//! it and Help › Copy Diagnostics Info puts it on the clipboard. One model,
//! one [`Diagnostics::report`], so a paste into an issue says exactly what
//! the pane says.
//!
//! The report is the family's (PwrGit's sections, PwrRoute's build lines
//! first). It never carries what was scanned: a scan's root, host and
//! volume names say who someone works for and what they keep, so the scan
//! is described by its kind and its counts only. PwrDisk's own folders are
//! shown with `~` for home.

use disktree_core::path_style::PathStyle;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::App;
use gpui_omarchy::ActiveTheme as _;

use crate::build_info::BuildInfo;
use crate::marks::display_path;
use crate::state::Disktree;

/// One line of a section: "Label: value".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub label: &'static str,
    pub value: String,
}

/// A titled group of facts: a card in the pane, a `[Title]` block in the
/// report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub facts: Vec<Fact>,
}

/// Everything the pane draws and the report writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostics {
    /// When this was read, as an RFC 3339 local time.
    pub collected: String,
    pub build: BuildInfo,
    pub sections: Vec<Section>,
    /// The log file, for the pane's Open Logs; `None` when there is none.
    pub log: Option<PathBuf>,
    /// The settings folder, for the pane's Open settings folder.
    pub settings_dir: Option<PathBuf>,
}

/// The facts the model is built from, read from the process and the OS
/// by [`collect`] and contrived by the tests.
#[derive(Clone, Debug)]
pub struct Facts {
    pub collected: String,
    pub build: BuildInfo,
    pub os: String,
    pub home: Option<PathBuf>,
    /// `Some(version)` when Velopack installed this copy.
    pub installed: Option<String>,
    /// "Stable · Latest (follows this build)".
    pub channel: String,
    /// The last check's outcome, or `None` before the first.
    pub last_check: Option<String>,
    pub settings_dir: Option<PathBuf>,
    pub log: Option<PathBuf>,
    /// The main window's, when it is open.
    pub session: Option<Session>,
}

/// What the main window is doing, without a single path in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub remote: bool,
    pub apparent_size: bool,
    pub scan: ScanState,
    pub files: Option<u64>,
    pub elapsed: Option<Duration>,
    /// "Balanced (8 workers)".
    pub power: String,
    pub theme: String,
    pub follows_system: bool,
    pub zoom_percent: u32,
    pub marked: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanState {
    /// The source chooser is up, or nothing has been scanned yet.
    None,
    Running,
    Complete,
    /// The message is left out: it can name the path that failed.
    Failed,
}

fn fact(label: &'static str, value: impl Into<String>) -> Fact {
    Fact {
        label,
        value: value.into(),
    }
}

/// The model from its parts.
pub fn build(facts: &Facts) -> Diagnostics {
    let home = facts.home.as_deref();
    let shown = |path: Option<&Path>, none: &str| {
        path.map_or_else(
            || none.to_owned(),
            |path| display_path(path, home, PathStyle::Native),
        )
    };
    let mut sections = vec![
        Section {
            title: "Platform",
            facts: vec![fact("Operating system", facts.os.clone())],
        },
        Section {
            title: "Updates",
            facts: vec![
                fact(
                    "Installed",
                    facts.installed.as_ref().map_or_else(
                        || {
                            "Not by an installer, so this copy does not \
                             update itself"
                                .to_owned()
                        },
                        |version| format!("Velopack {version}"),
                    ),
                ),
                fact("Channel", facts.channel.clone()),
                fact(
                    "Last check",
                    facts
                        .last_check
                        .clone()
                        .unwrap_or_else(|| "Not checked this session".into()),
                ),
            ],
        },
        Section {
            title: "Paths",
            facts: vec![
                fact(
                    "Settings folder",
                    shown(
                        facts.settings_dir.as_deref(),
                        "None: no home folder",
                    ),
                ),
                fact("Log", shown(facts.log.as_deref(), "None this session")),
            ],
        },
    ];
    if let Some(session) = &facts.session {
        sections.push(session_section(session));
    }
    Diagnostics {
        collected: facts.collected.clone(),
        build: facts.build.clone(),
        sections,
        log: facts.log.clone(),
        settings_dir: facts.settings_dir.clone(),
    }
}

fn session_section(session: &Session) -> Section {
    let source = if session.remote { "SSH" } else { "local" };
    let size = if session.apparent_size {
        "apparent size"
    } else {
        "allocated size"
    };
    let state = match session.scan {
        ScanState::None => "nothing scanned yet",
        ScanState::Running => "scanning",
        ScanState::Complete => "complete",
        ScanState::Failed => "failed",
    };
    let mut facts = vec![fact("Scan", format!("{source}, {size}, {state}"))];
    if let Some(files) = session.files {
        let count = thousands(files);
        facts.push(fact(
            "Files",
            session.elapsed.map_or_else(
                || count.clone(),
                |elapsed| format!("{count} in {:.1} s", elapsed.as_secs_f64()),
            ),
        ));
    }
    facts.push(fact("Marked", session.marked.to_string()));
    facts.push(fact("Power efficiency", session.power.clone()));
    let theme = if session.follows_system {
        format!("{}, follows the system appearance", session.theme)
    } else {
        format!("{}, from Omarchy", session.theme)
    };
    facts.push(fact("Theme", theme));
    facts.push(fact("Interface zoom", format!("{}%", session.zoom_percent)));
    Section {
        title: "Session",
        facts,
    }
}

impl Diagnostics {
    /// The clipboard's text: a heading, the build's lines (as About's Copy
    /// build info writes them), then each section as `[Title]` and
    /// "Label: value" lines.
    pub fn report(&self) -> String {
        let mut out = format!(
            "PwrDisk diagnostics, collected {}\n\n{}",
            self.collected,
            self.build.report()
        );
        for section in &self.sections {
            let _ = write!(out, "\n[{}]\n", section.title);
            for fact in &section.facts {
                let _ = writeln!(out, "{}: {}", fact.label, fact.value);
            }
        }
        out
    }

    /// "0.11.0 · release · aarch64 · ee680cbd": the pane's Version line.
    pub fn version_line(&self) -> String {
        let build = &self.build;
        let mut line = format!(
            "{} \u{b7} {} \u{b7} {}",
            build.version, build.profile, build.arch
        );
        if let Some(short) = build.short_commit_marked() {
            let _ = write!(line, " \u{b7} {short}");
        }
        line
    }
}

/// "1,204,331": exact, since a count in an issue is compared, not read.
/// The Logs window counts its lines with it too.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Reads this process, the OS and the main window.
pub fn collect(cx: &App) -> Diagnostics {
    use crate::updater::{self, Installation};
    let installed = match updater::installation() {
        Installation::Velopack { version } => Some(version),
        Installation::NotInstalled { .. } => None,
    };
    let installed_version = updater::installed_version();
    let selection = updater::selection();
    let slot = selection.resolve(&installed_version);
    let channel = if selection.is_inferred() {
        format!("{slot} (follows this build)")
    } else {
        format!("{slot} (picked)")
    };
    let state = updater::state();
    let last_check = if state.checking {
        Some("checking now".to_owned())
    } else {
        state.last.as_ref().map(|(_, outcome)| outcome.to_string())
    };
    let session = cx.windows().into_iter().find_map(|window| {
        let handle = window.downcast::<Disktree>()?;
        handle.read(cx).ok().map(|app| session(app, cx))
    });
    build(&Facts {
        collected: chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%:z")
            .to_string(),
        build: BuildInfo::current(),
        os: os_description(),
        home: std::env::home_dir(),
        installed,
        channel,
        last_check,
        settings_dir: updater::settings_dir(),
        log: crate::diag_log::path().map(Path::to_path_buf),
        session,
    })
}

fn session(app: &Disktree, cx: &App) -> Session {
    let scan = if app.scan.is_some() || app.remote_scan.is_some() {
        ScanState::Running
    } else if app.scan_error.is_some() {
        ScanState::Failed
    } else if app.tree.is_some() {
        ScanState::Complete
    } else {
        ScanState::None
    };
    let power = app.power_choice.unwrap_or_default();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the rem is a small positive zoom step"
    )]
    let zoom_percent = (app.rem / crate::ui::BASE_REM * 100.0).round() as u32;
    Session {
        remote: app.remote.is_some(),
        apparent_size: app.options.apparent_size,
        scan,
        files: app.tree.as_ref().map(|tree| tree.root().files),
        elapsed: app.scan_elapsed,
        power: power.label(app.cpu_threads),
        theme: cx.omarchy().name.to_string(),
        follows_system: crate::appearance::follows_system(app.home.as_deref()),
        zoom_percent,
        marked: app.marks.items().len(),
    }
}

/// "macOS 26.1 (aarch64)": the release from the system's own files, as
/// PwrRoute reads it; Windows by name only.
pub fn os_description() -> String {
    // Read once: Settings collects the diagnostics again whenever the
    // updater's state moves, and the OS does not change under a running
    // process.
    static DESCRIPTION: std::sync::OnceLock<String> =
        std::sync::OnceLock::new();
    DESCRIPTION.get_or_init(read_os_description).clone()
}

fn read_os_description() -> String {
    let arch = std::env::consts::ARCH;
    let version = if cfg!(target_os = "macos") {
        fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
            .ok()
            .and_then(|xml| plist_string(&xml, "ProductVersion"))
            .map(|version| format!("macOS {version}"))
    } else if cfg!(windows) {
        None
    } else {
        fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|text| os_release_name(&text))
    };
    let name = version.unwrap_or_else(|| match std::env::consts::OS {
        "macos" => "macOS".to_owned(),
        "windows" => "Windows".to_owned(),
        other => other.to_owned(),
    });
    format!("{name} ({arch})")
}

/// The `<string>` after `<key>key</key>` in a plist, without a parser: the
/// system version file is small and regular.
fn plist_string(xml: &str, key: &str) -> Option<String> {
    let after_key = &xml[xml.find(&format!("<key>{key}</key>"))?..];
    let start = after_key.find("<string>")? + "<string>".len();
    let end = after_key[start..].find("</string>")? + start;
    Some(after_key[start..end].trim().to_owned())
}

/// `PRETTY_NAME` from `/etc/os-release`, unquoted.
fn os_release_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            collected: "2026-10-30T00:12:03-04:00".to_owned(),
            build: BuildInfo::fixture(),
            os: "macOS 26.1 (aarch64)".to_owned(),
            home: Some(PathBuf::from("/Users/someone")),
            installed: Some("0.5.0".to_owned()),
            channel: "Stable \u{b7} Latest (follows this build)".to_owned(),
            last_check: None,
            settings_dir: Some(PathBuf::from(
                "/Users/someone/Library/Application Support/pwrdisk",
            )),
            log: Some(PathBuf::from(
                "/Users/someone/Library/Logs/PwrDisk/main.log",
            )),
            session: Some(Session {
                remote: true,
                apparent_size: false,
                scan: ScanState::Complete,
                files: Some(1_204),
                elapsed: Some(Duration::from_millis(14_230)),
                power: "Balanced (8 workers)".to_owned(),
                theme: "Tokyo Night".to_owned(),
                follows_system: true,
                zoom_percent: 100,
                marked: 3,
            }),
        }
    }

    #[test]
    fn the_report_leads_with_the_build_then_each_section() {
        let report = build(&facts()).report();
        assert!(
            report.starts_with(
                "PwrDisk diagnostics, collected 2026-10-30T00:12:03-04:00\n\n\
                 PwrDisk 0.5.0 (debug, aarch64)\n"
            ),
            "{report}"
        );
        // Diagnostics describes this client, even for an SSH session, so
        // every separator in its own paths follows the client.
        let sep = std::path::MAIN_SEPARATOR;
        let settings = format!(
            "Settings folder: ~{sep}Library{sep}Application Support{sep}pwrdisk\n"
        );
        let log =
            format!("Log: ~{sep}Library{sep}Logs{sep}PwrDisk{sep}main.log\n");
        for line in [
            "\n[Platform]\nOperating system: macOS 26.1 (aarch64)\n",
            "\n[Updates]\nInstalled: Velopack 0.5.0\n",
            "Last check: Not checked this session\n",
            &settings,
            &log,
            "\n[Session]\nScan: SSH, allocated size, complete\n",
            "Files: 1,204 in 14.2 s\n",
            "Marked: 3\n",
            "Theme: Tokyo Night, follows the system appearance\n",
            "Interface zoom: 100%\n",
        ] {
            assert!(report.contains(line), "{line:?} missing from\n{report}");
        }
    }

    #[test]
    fn nothing_scanned_is_ever_in_the_report() {
        // The session carries no path field at all; the only paths are
        // PwrDisk's own, shown from home.
        let report = build(&facts()).report();
        assert!(!report.contains("/Users/someone"), "{report}");
        let without_window = Facts {
            session: None,
            ..facts()
        };
        assert!(!build(&without_window).report().contains("[Session]"));
    }

    #[test]
    fn a_copy_not_installed_by_velopack_says_it_does_not_update() {
        let facts = Facts {
            installed: None,
            ..facts()
        };
        assert!(
            build(&facts)
                .report()
                .contains("Installed: Not by an installer")
        );
    }

    #[test]
    fn counts_are_exact_with_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_204_331), "1,204,331");
    }

    #[test]
    fn the_version_line_carries_the_short_commit() {
        assert_eq!(
            build(&facts()).version_line(),
            "0.5.0 \u{b7} debug \u{b7} aarch64 \u{b7} 5e1c07a2"
        );
    }

    #[test]
    fn the_os_is_read_from_the_system_files() {
        let plist = "<dict><key>ProductName</key><string>macOS</string>\
                     <key>ProductVersion</key><string>26.1</string></dict>";
        assert_eq!(
            plist_string(plist, "ProductVersion").as_deref(),
            Some("26.1")
        );
        assert_eq!(
            os_release_name("NAME=Arch\nPRETTY_NAME=\"Arch Linux\"\n")
                .as_deref(),
            Some("Arch Linux")
        );
        assert!(
            os_description()
                .ends_with(&format!("({})", std::env::consts::ARCH))
        );
    }
}
