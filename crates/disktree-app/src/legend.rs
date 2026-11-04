//! The legend as a filter. Each chip is shown, only, or hidden, and a click
//! moves it to the next: nothing set shows everything, a chip set to only
//! shows just that, and a hidden chip takes its bytes out.
//!
//! Kinds combine with OR and Suggested with AND: "Code" and "Cache" show
//! both, and Suggested narrows that to their cleanup suggestions. The age
//! chips filter the same way, and both sets apply in every mode, so "old
//! code" is one click in each.

use std::sync::Arc;

use disktree_core::classify::Category;
use disktree_core::filter::{Keep, Rule, filter_with, shares};
use gpui_kit::Context;

use crate::palette::AGE_BUCKETS;
use crate::state::Disktree;

/// Where one chip stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Chip {
    #[default]
    Shown,
    Only,
    Hidden,
}

impl Chip {
    /// What the next click makes it.
    pub const fn next(self) -> Self {
        match self {
            Self::Shown => Self::Only,
            Self::Only => Self::Hidden,
            Self::Hidden => Self::Shown,
        }
    }

    /// The state as an assistive technology reads it.
    pub const fn spoken(self) -> &'static str {
        match self {
            Self::Shown => "shown",
            Self::Only => "only",
            Self::Hidden => "hidden",
        }
    }
}

/// Which chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipId {
    Reclaimable,
    /// A kind, by its index in [`Category::LEGEND`].
    Kind(usize),
    /// An age, by its index in [`AGE_BUCKETS`].
    Age(usize),
}

impl ChipId {
    pub const fn label(self) -> &'static str {
        match self {
            // Suggested, not reclaimable: a hatch is a heuristic for review,
            // never a promise that the space can be had back.
            Self::Reclaimable => "Suggested",
            Self::Kind(index) => Category::LEGEND[index].label(),
            Self::Age(index) => AGE_BUCKETS[index].1,
        }
    }

    /// A stable name for element ids and tests.
    pub fn key(self) -> String {
        match self {
            Self::Reclaimable => "legend-reclaimable".into(),
            Self::Kind(index) => format!("legend-kind-{index}"),
            Self::Age(index) => format!("legend-age-{index}"),
        }
    }
}

/// Every chip's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LegendFilter {
    pub reclaimable: Chip,
    pub kinds: [Chip; Category::LEGEND.len()],
    pub ages: [Chip; AGE_BUCKETS.len()],
}

impl LegendFilter {
    pub const fn get(&self, id: ChipId) -> Chip {
        match id {
            ChipId::Reclaimable => self.reclaimable,
            ChipId::Kind(index) => self.kinds[index],
            ChipId::Age(index) => self.ages[index],
        }
    }

    const fn slot(&mut self, id: ChipId) -> &mut Chip {
        match id {
            ChipId::Reclaimable => &mut self.reclaimable,
            ChipId::Kind(index) => &mut self.kinds[index],
            ChipId::Age(index) => &mut self.ages[index],
        }
    }

    pub const fn cycle(&mut self, id: ChipId) {
        let slot = self.slot(id);
        *slot = slot.next();
    }

    pub fn is_active(&self) -> bool {
        self.reclaimable != Chip::Shown
            || self.kinds_active()
            || self.ages_active()
    }

    pub fn kinds_active(&self) -> bool {
        self.kinds.iter().any(|chip| *chip != Chip::Shown)
    }

    pub fn ages_active(&self) -> bool {
        self.ages.iter().any(|chip| *chip != Chip::Shown)
    }

    /// Whether any chip of the same set as `id` is set to only, which
    /// fades the ones that are not.
    pub fn narrowed(&self, id: ChipId) -> bool {
        match id {
            ChipId::Reclaimable => false,
            ChipId::Kind(_) => self.kinds.contains(&Chip::Only),
            ChipId::Age(_) => self.ages.contains(&Chip::Only),
        }
    }

    pub fn clear_kinds(&mut self) {
        self.kinds = Default::default();
    }

    pub fn clear_ages(&mut self) {
        self.ages = Default::default();
    }

    /// The rule a filtered view applies, with ages measured from `now`.
    pub fn rule(&self, now: i64) -> Rule {
        let bits = |chips: &[Chip], wanted: Chip| {
            chips
                .iter()
                .enumerate()
                .filter(|(_, chip)| **chip == wanted)
                .fold(0_u16, |bits, (index, _)| {
                    bits | Rule::bit(Category::LEGEND[index])
                })
        };
        let ages = |wanted: Chip| {
            self.ages
                .iter()
                .enumerate()
                .filter(|(_, chip)| **chip == wanted)
                .fold(0_u8, |bits, (index, _)| bits | 1 << index)
        };
        Rule {
            only: bits(&self.kinds, Chip::Only),
            hidden: bits(&self.kinds, Chip::Hidden),
            reclaimable: match self.reclaimable {
                Chip::Shown => None,
                Chip::Only => Some(true),
                Chip::Hidden => Some(false),
            },
            ages_only: ages(Chip::Only),
            ages_hidden: ages(Chip::Hidden),
            age_limits: AGE_BUCKETS.iter().map(|(limit, _)| *limit).collect(),
            now,
        }
    }

    /// What the kind chips keep, in words: "Code, Cache", "Media
    /// hidden", or both. `None` when they keep everything.
    pub fn kinds_summary(&self) -> Option<String> {
        let labels = |wanted: Chip| {
            Category::LEGEND
                .iter()
                .zip(&self.kinds)
                .filter(|(_, chip)| **chip == wanted)
                .map(|(category, _)| category.label())
                .collect::<Vec<_>>()
        };
        summary(&labels(Chip::Only), &labels(Chip::Hidden))
    }

    /// What the age chips keep, in words.
    pub fn ages_summary(&self) -> Option<String> {
        let labels = |wanted: Chip| {
            AGE_BUCKETS
                .iter()
                .zip(&self.ages)
                .filter(|(_, chip)| **chip == wanted)
                .map(|((_, label), _)| *label)
                .collect::<Vec<_>>()
        };
        summary(&labels(Chip::Only), &labels(Chip::Hidden))
    }

    /// Everything the legend keeps, for the totals line: "suggested
    /// Code, Cache · Older".
    pub fn summary(&self) -> String {
        let kinds = self.kinds_summary();
        let mut parts = Vec::new();
        match (self.reclaimable, kinds) {
            (Chip::Only, Some(kinds)) => {
                parts.push(format!("suggested {kinds}"));
            }
            (Chip::Only, None) => parts.push("suggested".to_owned()),
            (Chip::Hidden, kinds) => {
                parts.extend(kinds);
                parts.push("suggested hidden".to_owned());
            }
            (Chip::Shown, kinds) => parts.extend(kinds),
        }
        parts.extend(self.ages_summary());
        parts.join(" · ")
    }

    /// What the next click on `id` does, for its tooltip.
    pub fn hint(&self, id: ChipId) -> String {
        let label = id.label();
        match self.get(id) {
            Chip::Shown => format!("Click to show only {label}"),
            Chip::Only => format!("Showing only {label} · click to hide it"),
            Chip::Hidden => format!("{label} hidden · click to show it again"),
        }
    }
}

/// "A, B" for chips set to only, "C hidden" for hidden ones, or both.
fn summary(only: &[&str], hidden: &[&str]) -> Option<String> {
    let mut parts = Vec::new();
    if !only.is_empty() {
        parts.push(only.join(", "));
    }
    if !hidden.is_empty() {
        parts.push(format!("{} hidden", hidden.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl Disktree {
    /// A click on a legend chip.
    pub fn cycle_legend(&mut self, id: ChipId, cx: &mut Context<'_, Self>) {
        self.legend.cycle(id);
        self.legend_changed(cx);
    }

    /// Show all, or the pill for the chips not on screen.
    pub fn clear_legend(
        &mut self,
        part: Option<ChipId>,
        cx: &mut Context<'_, Self>,
    ) {
        match part {
            None => self.legend = LegendFilter::default(),
            Some(ChipId::Kind(_)) => self.legend.clear_kinds(),
            Some(ChipId::Age(_)) => self.legend.clear_ages(),
            Some(ChipId::Reclaimable) => self.legend.reclaimable = Chip::Shown,
        }
        self.legend_changed(cx);
    }

    fn legend_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.refresh_legend(cx);
        // A search keeps to what the legend keeps, so it runs again.
        if !self.find.trim().is_empty() {
            self.refresh_matches(cx);
        }
        cx.notify();
    }

    /// When the legend leaves nothing of the directory on screen: what it
    /// keeps in the whole scan, bytes and files, so the view can say
    /// whether going elsewhere would show anything. `None` while there is
    /// something to show, or while the filter is still being worked out.
    pub fn legend_empty_here(&self) -> Option<(u64, u64)> {
        if self.filter_applied
            || self.legend_pending
            || !self.legend.is_active()
        {
            return None;
        }
        let matches = self.legend_matches.as_deref()?;
        match matches.keep(&self.crumbs) {
            // The scanned root is never recorded, only summed: nothing
            // passed when that sum is empty.
            None | Some(Keep::Partial { bytes: 0, files: 0 }) => {
                Some((matches.bytes, matches.files))
            }
            Some(_) => None,
        }
    }

    /// The rule the legend sets, when it sets one. Ages are measured from
    /// the scan, as the age colours are, so a chip keeps exactly the tiles
    /// drawn in its colour however long the window has been open.
    pub fn legend_rule(&self) -> Option<Rule> {
        self.legend
            .is_active()
            .then(|| self.legend.rule(self.scanned_at))
    }

    /// Work out what each chip would keep of the whole scan, off the UI
    /// thread, for the sizes beside the chips. Once per tree: ages are
    /// measured from the scan, as the age colours are.
    pub fn refresh_shares(&mut self, cx: &Context<'_, Self>) {
        self.shares_epoch += 1;
        let epoch = self.shares_epoch;
        let Some(tree) = self.tree.clone() else {
            self.legend_shares = None;
            return;
        };
        let now = self.scanned_at;
        let task = cx.background_executor().spawn(async move {
            let limits: Vec<i64> =
                AGE_BUCKETS.iter().map(|(limit, _)| *limit).collect();
            shares(tree.root(), now, &limits)
        });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |this, cx| {
                if epoch == this.shares_epoch {
                    this.legend_shares = Some(Arc::new(found));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Work out what the legend keeps, from the scanned root down, off the
    /// UI thread. Once, not per directory: moving around a filtered view
    /// then needs no search, and every directory's share is already known.
    pub fn refresh_legend(&mut self, cx: &Context<'_, Self>) {
        self.legend_epoch += 1;
        let epoch = self.legend_epoch;
        let (Some(rule), Some(tree)) = (self.legend_rule(), self.tree.clone())
        else {
            self.legend_matches = None;
            self.legend_pending = false;
            self.filter_epoch += 1;
            return;
        };
        self.legend_pending = true;
        let task = cx.background_executor().spawn(async move {
            filter_with(tree.root(), &[], "", Some(&rule))
        });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |this, cx| {
                if epoch != this.legend_epoch {
                    return;
                }
                this.legend_pending = false;
                this.legend_matches = found.map(Arc::new);
                this.filter_epoch += 1;
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chip_goes_only_then_hidden_then_back() {
        let mut legend = LegendFilter::default();
        let code = ChipId::Kind(0);
        assert!(!legend.is_active());
        legend.cycle(code);
        assert_eq!(legend.get(code), Chip::Only);
        assert!(legend.narrowed(ChipId::Kind(3)));
        assert!(!legend.narrowed(ChipId::Age(0)));
        legend.cycle(code);
        assert_eq!(legend.get(code), Chip::Hidden);
        assert!(!legend.narrowed(ChipId::Kind(3)));
        legend.cycle(code);
        assert!(!legend.is_active());
    }

    #[test]
    fn the_rule_and_the_words_say_the_same_thing() {
        let mut legend = LegendFilter::default();
        legend.cycle(ChipId::Kind(0));
        legend.cycle(ChipId::Kind(7));
        let rule = legend.rule(0);
        assert_eq!(
            rule.only,
            Rule::bit(Category::Code) | Rule::bit(Category::Cache)
        );
        assert_eq!(legend.summary(), "Code, Cache");

        legend.cycle(ChipId::Reclaimable);
        assert_eq!(legend.rule(0).reclaimable, Some(true));
        assert_eq!(legend.summary(), "suggested Code, Cache");

        let mut legend = LegendFilter::default();
        legend.cycle(ChipId::Kind(5));
        legend.cycle(ChipId::Kind(5));
        legend.cycle(ChipId::Age(4));
        assert_eq!(legend.rule(0).hidden, Rule::bit(Category::Media));
        assert_eq!(legend.rule(0).ages_only, 1 << 4);
        assert_eq!(legend.summary(), "Media hidden · Older");
        assert_eq!(legend.kinds_summary().as_deref(), Some("Media hidden"));
    }
}
