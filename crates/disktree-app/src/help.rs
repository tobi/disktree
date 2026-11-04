//! Help: the menu's rows and what each does. Links to the project open in
//! the default browser; Changelog, the license, the notices and the log
//! open in PwrDisk's own windows (`doc_window`, `logs_window`) and work
//! offline. Copy Diagnostics Info writes Settings › Diagnostics' report
//! (`diagnostics`) and says so in a toast in the main window.

use gpui_kit::{App, ClipboardItem};

use crate::doc_window::{self, Doc};

pub use help_actions::*;

#[allow(
    clippy::derive_partial_eq_without_eq,
    reason = "GPUI's actions! macro writes the derives, not us"
)]
mod help_actions {
    use gpui_kit::actions;

    actions!(
        disktree,
        [
            /// The README on GitHub: PwrDisk's documentation for now.
            OpenDocumentation,
            /// CHANGELOG.md in its own window.
            OpenChangelog,
            /// GitHub's new-issue page.
            ReportIssue,
            /// GitHub's private vulnerability report.
            ReportVulnerability,
            /// Diagnostics' lines on the clipboard.
            CopyDiagnostics,
            /// The diagnostic log in its own window.
            OpenLogs,
            /// The repository on GitHub.
            ViewSource,
            /// LICENSE in its own window.
            ViewLicense,
            /// THIRD_PARTY_LICENSES in its own window.
            ThirdPartyNotices,
        ]
    );
}

/// The repository, from the workspace's `repository`.
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

pub fn documentation_url() -> String {
    format!("{REPOSITORY}#readme")
}

pub fn issue_url() -> String {
    format!("{REPOSITORY}/issues/new")
}

/// GitHub's private reporting, as the family's SECURITY.md asks for.
pub fn vulnerability_url() -> String {
    format!("{REPOSITORY}/security/advisories/new")
}

pub fn install(cx: &mut App) {
    cx.on_action(|_: &OpenDocumentation, cx| {
        cx.open_url(&documentation_url());
    });
    cx.on_action(|_: &ReportIssue, cx| cx.open_url(&issue_url()));
    cx.on_action(|_: &ReportVulnerability, cx| {
        cx.open_url(&vulnerability_url());
    });
    cx.on_action(|_: &ViewSource, cx| cx.open_url(REPOSITORY));
    // The windows wait for the dispatch to finish: chosen from inside the
    // window it would bring forward, GPUI still holds it, so it could be
    // neither read nor raised and a second would open.
    let doc = |doc: Doc, cx: &mut App| {
        cx.defer(move |cx| doc_window::open(doc, cx));
    };
    cx.on_action(move |_: &OpenChangelog, cx| doc(Doc::Changelog, cx));
    cx.on_action(move |_: &ViewLicense, cx| doc(Doc::License, cx));
    cx.on_action(move |_: &ThirdPartyNotices, cx| {
        doc(Doc::ThirdPartyNotices, cx);
    });
    cx.on_action(|_: &OpenLogs, cx| cx.defer(crate::logs_window::open));
    // Deferred: from the strip's menu the main window is still the one
    // dispatching, and could not be read for its session.
    cx.on_action(|_: &CopyDiagnostics, cx| cx.defer(copy_diagnostics));
}

/// Settings › Diagnostics' report on the clipboard, and a note in the
/// main window that it is there.
pub fn copy_diagnostics(cx: &mut App) {
    let report = crate::diagnostics::collect(cx).report();
    cx.write_to_clipboard(ClipboardItem::new_string(report));
    log::info!(target: "pwrdisk", "help: copied diagnostics info");
    let main = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<crate::state::Disktree>());
    if let Some(main) = main {
        cx.defer(move |cx| {
            let _ = main.update(cx, |app, _, cx| {
                app.say(
                    "Diagnostics copied \u{b7} paste them into an issue",
                    gpui_omarchy::Status::Success,
                    cx,
                );
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_point_at_the_repository() {
        assert_eq!(REPOSITORY, "https://github.com/pwrdrvr/PwrDisk");
        assert_eq!(
            documentation_url(),
            "https://github.com/pwrdrvr/PwrDisk#readme"
        );
        assert_eq!(
            issue_url(),
            "https://github.com/pwrdrvr/PwrDisk/issues/new"
        );
        assert_eq!(
            vulnerability_url(),
            "https://github.com/pwrdrvr/PwrDisk/security/advisories/new"
        );
    }
}
