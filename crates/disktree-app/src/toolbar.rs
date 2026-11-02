//! How much of the explore bar fits, and the ⋮ menu that holds the rest.
//!
//! The bar folds by what is changed least: the scan options first (they
//! apply on the next scan), then history and depth (which have keys), then
//! Size | Files | Age. The Treemap | List switch never folds.

use gpui_kit::Context;

use crate::state::Disktree;

/// Widest first. Measured in rems, not pixels, so interface zoom walks
/// through the tiers exactly as a narrower window does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum BarFit {
    /// Every control in the bar; no ⋮.
    #[default]
    Full,
    /// Hidden files, Apparent size and the scan workers go in ⋮.
    Folded,
    /// History and Depth join them.
    Narrow,
    /// Size | Files | Age too: only the view switch and ⋮ are left.
    Tight,
}

/// At and above this many rems across, the whole bar fits beside a
/// readable trail: the settings take about 51 rem, the stoplights, mark
/// and wordmark about 12, and the trail keeps at least 13.
const FULL_REMS: f32 = 76.0;
/// Below this the side panel gives way too (`PANEL_SHOWN_REMS`); it is
/// where the settings used to wrap onto a second row.
const NARROW_REMS: f32 = 52.0;
/// Below this Size | Files | Age (11 rem) would leave the trail no room.
const TIGHT_REMS: f32 = 40.0;
/// The switch keeps its words down to here; under it, glyphs alone.
const LABELLED_SWITCH_REMS: f32 = 64.0;

impl BarFit {
    pub fn for_width(rems: f32) -> Self {
        if rems >= FULL_REMS {
            Self::Full
        } else if rems >= NARROW_REMS {
            Self::Folded
        } else if rems >= TIGHT_REMS {
            Self::Narrow
        } else {
            Self::Tight
        }
    }

    pub fn labelled_switch(rems: f32) -> bool {
        rems >= LABELLED_SWITCH_REMS
    }

    pub fn shows_scan_options(self) -> bool {
        self == Self::Full
    }

    pub fn shows_history_and_depth(self) -> bool {
        self <= Self::Folded
    }

    pub fn shows_mode(self) -> bool {
        self <= Self::Narrow
    }

    /// The ⋮ menu's rows, top to bottom. The most-used come first, as they
    /// would read left to right in a wider bar.
    pub fn more_items(self) -> Vec<MoreItem> {
        let mut items = Vec::new();
        if !self.shows_mode() {
            items.extend([MoreItem::Mode(0), MoreItem::Mode(1)]);
            items.push(MoreItem::Mode(2));
        }
        if !self.shows_history_and_depth() {
            items.extend([MoreItem::Back, MoreItem::Forward, MoreItem::Depth]);
        }
        if !self.shows_scan_options() {
            items.extend([
                MoreItem::Hidden,
                MoreItem::Apparent,
                MoreItem::Workers,
            ]);
        }
        items
    }
}

/// One row of the ⋮ menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoreItem {
    /// Size, Files or Age, by its index in the segmented control.
    Mode(usize),
    Back,
    Forward,
    /// Depth, adjusted in place with ← → or the row's − and +.
    Depth,
    Hidden,
    Apparent,
    /// Opens the Power Efficiency menu in its place.
    Workers,
}

impl MoreItem {
    /// The section a row belongs to; a caption opens each.
    pub const fn section(self) -> &'static str {
        match self {
            Self::Mode(_) => "Measure by",
            Self::Back | Self::Forward | Self::Depth => "View",
            Self::Hidden | Self::Apparent | Self::Workers => "Scan",
        }
    }
}

impl Disktree {
    /// Treemap or file list. The list takes the keyboard when it appears, so
    /// a screen reader lands in it; the treemap gives it back to the root.
    pub fn show_file_list(&mut self, on: bool, cx: &mut Context<'_, Self>) {
        self.more_menu = None;
        if self.file_list != on {
            self.file_list = on;
            self.pointer_active = false;
        }
        cx.notify();
    }

    /// Open ⋮ on its first row, or close it.
    pub fn toggle_more_menu(&mut self, cx: &mut Context<'_, Self>) {
        self.crumb_menu = None;
        // The worker presets open in ⋮'s place; ⋮ then closes them.
        let was_open = self.more_menu.is_some() || self.power_menu.is_some();
        self.power_menu = None;
        self.more_menu = if was_open {
            None
        } else {
            self.next_enabled_more(&self.bar_fit.more_items(), None, true)
        };
        cx.notify();
    }

    /// The next row after `from` (or the first, from `None`) that can act,
    /// so the highlight never rests on a disabled Back or Forward.
    fn next_enabled_more(
        &self,
        items: &[MoreItem],
        from: Option<usize>,
        forward: bool,
    ) -> Option<usize> {
        let enabled = |&index: &usize| self.more_item_enabled(items[index]);
        match (from, forward) {
            (None, _) => (0..items.len()).find(enabled),
            (Some(at), true) => (at + 1..items.len()).find(enabled),
            (Some(at), false) => (0..at).rev().find(enabled),
        }
    }

    /// Whether a ⋮ row can act now. History rows wait for somewhere to go.
    pub fn more_item_enabled(&self, item: MoreItem) -> bool {
        match item {
            MoreItem::Back => self.can_go_back(),
            MoreItem::Forward => self.can_go_forward(),
            _ => true,
        }
    }

    /// Do what a ⋮ row says. Every row but Depth closes the menu.
    pub fn activate_more(
        &mut self,
        item: MoreItem,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.more_item_enabled(item) {
            return;
        }
        if item != MoreItem::Depth {
            self.more_menu = None;
        }
        match item {
            MoreItem::Mode(index) => self.set_mode(index, cx),
            MoreItem::Back => self.go_back(cx),
            MoreItem::Forward => self.go_forward(cx),
            MoreItem::Depth => {}
            MoreItem::Hidden => self.toggle_hidden(cx),
            MoreItem::Apparent => self.toggle_apparent(cx),
            MoreItem::Workers => self.toggle_power_menu(cx),
        }
        cx.notify();
    }

    /// Keys while ⋮ is open, as the sibling and power menus take them. Any
    /// other key closes it and goes on to the screen.
    pub fn on_more_key(
        &mut self,
        key: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(highlighted) = self.more_menu else {
            return false;
        };
        let items = self.bar_fit.more_items();
        let Some(&item) = items.get(highlighted) else {
            self.more_menu = None;
            return false;
        };
        let step = |forward: bool| {
            self.next_enabled_more(&items, Some(highlighted), forward)
                .unwrap_or(highlighted)
        };
        match key {
            "down" | "j" => self.more_menu = Some(step(true)),
            "up" | "k" => self.more_menu = Some(step(false)),
            "home" => {
                self.more_menu = self.next_enabled_more(&items, None, true);
            }
            "end" => {
                self.more_menu = (0..items.len())
                    .rev()
                    .find(|&index| self.more_item_enabled(items[index]));
            }
            "left" | "h" | "-" if item == MoreItem::Depth => {
                self.adjust_depth(-1, cx);
            }
            "right" | "l" | "+" | "=" if item == MoreItem::Depth => {
                self.adjust_depth(1, cx);
            }
            "enter" | "space" => self.activate_more(item, cx),
            "escape" => self.more_menu = None,
            _ => {
                self.more_menu = None;
                cx.notify();
                return false;
            }
        }
        cx.notify();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_folds_least_used_first() {
        assert_eq!(BarFit::for_width(90.0), BarFit::Full);
        assert_eq!(BarFit::for_width(76.0), BarFit::Full);
        // 1100 px and the 900 px minimum window, at 100%.
        assert_eq!(BarFit::for_width(68.75), BarFit::Folded);
        assert_eq!(BarFit::for_width(56.25), BarFit::Folded);
        assert_eq!(BarFit::for_width(45.0), BarFit::Narrow);
        // The minimum window at 175%.
        assert_eq!(BarFit::for_width(32.0), BarFit::Tight);

        assert!(BarFit::Full.more_items().is_empty());
        assert_eq!(
            BarFit::Folded.more_items(),
            [MoreItem::Hidden, MoreItem::Apparent, MoreItem::Workers]
        );
        assert_eq!(BarFit::Narrow.more_items().len(), 6);
        assert_eq!(BarFit::Tight.more_items()[0], MoreItem::Mode(0));
        assert_eq!(BarFit::Tight.more_items().len(), 9);
    }

    #[test]
    fn the_switch_keeps_its_words_down_to_64_rem() {
        assert!(BarFit::labelled_switch(68.75));
        assert!(!BarFit::labelled_switch(56.25));
    }
}
