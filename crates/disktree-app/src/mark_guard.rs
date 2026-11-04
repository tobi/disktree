//! What may be marked, and what Review asks before a removal past the
//! limit.
//!
//! The guards in `removal.rs` decide what may be removed; asking them only
//! in Review let a filled Mark for removal sit on `/Users`. So a mark is
//! judged when it is made: a protected path is never marked, and a mark
//! that takes a volume past Settings › Safety's limit needs Mark anyway
//! here and a second confirmation in Review.
//!
//! Review counts each disk on its own, as the worker does: a scan can hold
//! marks on a disk mounted inside it, and a share confirmed for one disk
//! says nothing about another.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use disktree_core::removal::{Plan, RemovalMode, Target};
use disktree_core::removal_limit::{Ceilings, Limit, Usage};
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::{App, AppContext as _, Context, Focusable as _, Window};
use gpui_omarchy::Status;

use crate::safety::{Confirm, Policy, matches, phrase, share_percent};
use crate::state::Disktree;

/// Whether a new mark may be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkCheck {
    Allowed,
    /// The guards refuse it, with their reason. No setting lifts this.
    Protected(String),
    /// Marking it takes the volume's marked total past the limit.
    OverLimit(Share),
}

/// Marked bytes as a share of the scanned volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    pub bytes: u64,
    pub capacity: u64,
    /// Rounded up; see [`share_percent`].
    pub percent: u8,
    pub limit: u8,
}

impl Share {
    fn over(bytes: u64, capacity: u64, limit: Limit) -> Option<Self> {
        // `bytes > floor(capacity * limit / 100)`, as the worker's check
        // counts it, without the rounding.
        (u128::from(bytes) * 100
            > u128::from(capacity) * u128::from(limit.percent()))
        .then(|| Self {
            bytes,
            capacity,
            percent: share_percent(bytes, capacity),
            limit: limit.percent(),
        })
    }

    /// The share unrounded, for ordering two disks whose rounded shares
    /// tie: parts per million of capacity.
    fn bytes_share(&self) -> u128 {
        u128::from(self.bytes) * 1_000_000 / u128::from(self.capacity.max(1))
    }
}

/// One path's answer from the guards, kept while the inspector stays on
/// it: judging reads the filesystem, and the inspector draws every frame.
pub type RefusalCache = Option<(PathBuf, PathBuf, Option<String>)>;

/// The disk removing a path gives space back on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// The worker's name for it, from
    /// [`disktree_core::removal_limit::volume_of`]: the key a confirmed
    /// share is raised under.
    pub key: String,
    pub capacity: u64,
    /// Where it is mounted, for the person to read.
    pub name: String,
}

/// How Review finds a path's disk; [`disk_of`] outside tests.
pub type DiskProbe = fn(&Path) -> Result<Disk, String>;

/// The probe's answers by marked path, for Review's frames: the probe
/// reads the filesystem, and Review draws the gate every frame.
pub type DiskCache = HashMap<PathBuf, Result<Disk, String>>;

/// The disk `path` is on, as the worker's check counts it, named by
/// where it is mounted. Both come from the path the check reads, so a
/// marked link is named by the disk it sits on, not the one it points to.
pub fn disk_of(path: &Path) -> Result<Disk, String> {
    let entry = disktree_core::removal_limit::volume_entry(path)?;
    let (key, capacity) = disktree_core::removal_limit::volume_of(entry)?;
    let name = disktree_core::space::volume_root_for(entry)
        .map_or_else(|| key.clone(), |root| root.display().to_string());
    Ok(Disk {
        key,
        capacity,
        name,
    })
}

/// One disk a removal takes past the limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Over {
    pub key: String,
    pub name: String,
    pub share: Share,
}

/// Review's second confirmation: every disk a removal takes past the
/// window's limit, the largest share first, as the phrase reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    pub over: Vec<Over>,
    pub limit: u8,
}

/// What a tick confirmed: each disk's share, and the mode.
pub type Ticked = (Vec<(String, u8)>, RemovalMode);

impl Gate {
    /// The disks of `usage` past `limit`; `None` when none is.
    fn new(
        usage: Vec<Usage>,
        names: &BTreeMap<String, String>,
        limit: Limit,
    ) -> Option<Self> {
        let mut over: Vec<Over> = usage
            .into_iter()
            .filter_map(
                |Usage {
                     volume,
                     bytes,
                     capacity,
                 }| {
                    let share = Share::over(bytes, capacity, limit)?;
                    let name = names
                        .get(&volume)
                        .cloned()
                        .unwrap_or_else(|| volume.clone());
                    Some(Over {
                        key: volume,
                        name,
                        share,
                    })
                },
            )
            .collect();
        // Stable, so equal shares keep the worker's key order.
        over.sort_by_key(|over| std::cmp::Reverse(over.share.bytes_share()));
        (!over.is_empty()).then(|| Self {
            over,
            limit: limit.percent(),
        })
    }

    /// Each disk's share, in the gate's order.
    pub fn percents(&self) -> Vec<u8> {
        self.over.iter().map(|over| over.share.percent).collect()
    }

    /// "30% of /" or "31% of / and 30% of /Volumes/USB".
    pub fn summary(&self) -> String {
        crate::safety::series(
            self.over.iter().map(|over| {
                format!("{}% of {}", over.share.percent, over.name)
            }),
        )
    }

    /// What a tick of this gate in `mode` confirms.
    pub fn ticked(&self, mode: RemovalMode) -> Ticked {
        (
            self.over
                .iter()
                .map(|over| (over.key.clone(), over.share.percent))
                .collect(),
            mode,
        )
    }

    /// `base`, with each disk here raised to its confirmed share and no
    /// other: a mark on a disk this gate does not name stays at `base`.
    fn ceilings(&self, base: Limit) -> Result<Ceilings, String> {
        self.over
            .iter()
            .try_fold(Ceilings::from(base), |ceilings, over| {
                Ok(ceilings.raise(
                    over.key.clone(),
                    Limit::new(over.share.percent.max(1))?,
                ))
            })
    }
}

impl Disktree {
    /// Settings › Safety as last read. Read once and again before every
    /// removal, never per frame; Settings saves it, and an unreadable file
    /// is the default here, which is the stricter reading.
    pub fn policy(&self) -> Policy {
        *self.safety_policy.borrow_mut().get_or_insert_with(|| {
            crate::safety::load_policy(self.removal_settings_dir.as_deref())
                .unwrap_or_default()
        })
    }

    /// Read Settings › Safety again, as a removal must.
    pub fn reload_policy(&self) -> Result<Policy, String> {
        let policy =
            crate::safety::load_policy(self.removal_settings_dir.as_deref());
        *self.safety_policy.borrow_mut() =
            Some(policy.clone().unwrap_or_default());
        policy
    }

    /// Why the guards would refuse `path`. An SSH host's paths are never
    /// judged against this computer's filesystem: nothing is removed
    /// there, and its marks only export a list.
    pub fn refusal_of(&self, path: &Path) -> Option<String> {
        if self.remote.is_some() {
            return None;
        }
        if let Some((root, cached, reason)) = &*self.refusal_cache.borrow()
            && *root == self.root_path
            && cached == path
        {
            return reason.clone();
        }
        let reason = disktree_core::removal::refusal(path, &self.root_path);
        *self.refusal_cache.borrow_mut() =
            Some((self.root_path.clone(), path.to_path_buf(), reason.clone()));
        reason
    }

    /// The marked total with `target` marked too: marks inside it are
    /// absorbed by it, so they are not counted twice.
    fn marked_with(&self, target: &Target) -> u64 {
        let style = self.path_style();
        self.marks
            .items()
            .iter()
            .filter(|mark| {
                mark.path != target.path
                    && !style.is_below(&mark.path, &target.path)
            })
            .map(|mark| mark.bytes)
            .fold(target.bytes, u64::saturating_add)
    }

    /// Whether marking `target` passes `limit` on the scanned volume.
    /// Every mark counts against it, wherever it lives, so a mark on
    /// another disk can be over- or under-counted here: a small disk
    /// mounted inside the scan passes its own limit long before the scanned
    /// one does. Review and the worker count each disk on its own, and
    /// those are what hold a removal to the limit.
    fn over_limit(&self, target: &Target, limit: Limit) -> Option<Share> {
        Share::over(self.marked_with(target), self.capacity()?, limit)
    }

    /// The scanned volume's capacity: the ticker's, or measured now when
    /// it has no reading yet, so a mark made before the first reading is
    /// still judged.
    fn capacity(&self) -> Option<u64> {
        self.space
            .or_else(|| disktree_core::space::space_info(&self.root_path).ok())
            .map(|space| space.total)
    }

    /// Whether `target`, not yet marked, may be marked now.
    pub fn mark_check(&self, target: &Target) -> MarkCheck {
        if let Some(reason) = self.refusal_of(&target.path) {
            return MarkCheck::Protected(reason);
        }
        if self.remote.is_some() {
            return MarkCheck::Allowed;
        }
        self.policy()
            .limit
            .and_then(|limit| self.over_limit(target, limit))
            .map_or(MarkCheck::Allowed, MarkCheck::OverLimit)
    }

    /// Why an agent may not mark `target`, if it may not. An agent is held
    /// to the limit with no way past it, and to the default one when the
    /// window's is off: going past it is the person's call, in the window.
    pub fn agent_mark_refusal(&self, target: &Target) -> Option<String> {
        if let Some(reason) = self.refusal_of(&target.path) {
            return Some(reason);
        }
        let limit = self.policy().agent_limit();
        self.over_limit(target, limit).map(|share| {
            format!(
                "marking it would put {}% of the volume in the list, over \
                 the {}% limit; only the person can mark past it, in the \
                 window",
                share.percent, share.limit
            )
        })
    }

    /// The notice for a mark refused here; `None` when it was allowed.
    pub fn mark_refusal_notice(
        &self,
        target: &Target,
        check: &MarkCheck,
    ) -> Option<String> {
        let name = self.display_path(&target.path);
        match check {
            MarkCheck::Allowed => None,
            MarkCheck::Protected(reason) => Some(format!("{name}: {reason}")),
            MarkCheck::OverLimit(share) => Some(format!(
                "{name} would make the marked list {}% of this disk, over \
                 the {}% limit; use \u{25be} beside Mark to mark it anyway",
                share.percent, share.limit
            )),
        }
    }

    /// The inspector's Mark anyway: the one way to mark past the limit.
    /// Protected paths stay refused.
    pub fn mark_anyway(
        &mut self,
        crumbs: &[usize],
        cx: &mut Context<'_, Self>,
    ) {
        self.mark_menu = None;
        self.toggle_mark_checked(crumbs, true, cx);
    }

    /// The plan's bytes disk by disk, with each disk's name, as the
    /// worker's check groups them. `fresh` asks the probe again and keeps
    /// its answers; otherwise answers kept since Review opened are reused.
    fn usage(
        &self,
        plan: &Plan,
        fresh: bool,
    ) -> Result<(Vec<Usage>, BTreeMap<String, String>), String> {
        let mut cache = self.disk_cache.borrow_mut();
        let mut names = BTreeMap::new();
        let usage =
            disktree_core::removal_limit::usage(&plan.targets, |path| {
                let disk = if fresh {
                    let disk = (self.disk_probe)(path);
                    cache.insert(path.to_path_buf(), disk.clone());
                    disk
                } else {
                    cache
                        .entry(path.to_path_buf())
                        .or_insert_with(|| (self.disk_probe)(path))
                        .clone()
                }?;
                names.entry(disk.key.clone()).or_insert(disk.name);
                Ok((disk.key, disk.capacity))
            })?;
        Ok((usage, names))
    }

    /// Forget which disk each mark is on, so Review reads them again. Every
    /// way into Review does this: a disk mounted or unmounted since the
    /// last visit is then seen.
    pub fn reread_disks(&self) {
        self.disk_cache.borrow_mut().clear();
    }

    /// Every disk the plan takes past the window's limit, for Review's
    /// second confirmation. `None` with the limit off, over SSH, or when a
    /// disk cannot be read, where the worker's own check still stands.
    pub fn removal_gate(&self, plan: &Plan) -> Option<Gate> {
        if self.remote.is_some() {
            return None;
        }
        let limit = self.policy().limit?;
        let (usage, names) = self.usage(plan, false).ok()?;
        Gate::new(usage, &names, limit)
    }

    /// The phrase Review asks for `gate`, in the current mode.
    pub fn gate_phrase(&self, gate: &Gate) -> String {
        phrase(&gate.percents(), self.removal_mode)
    }

    /// Whether the person has confirmed `gate` as Settings asks.
    pub fn gate_met(&self, gate: &Gate, cx: &App) -> bool {
        match self.policy().confirm {
            Confirm::Nothing => true,
            Confirm::Tick => {
                self.over_ticked.as_ref()
                    == Some(&gate.ticked(self.removal_mode))
            }
            Confirm::Phrase => self.over_phrase.as_ref().is_some_and(|input| {
                matches(&input.read(cx).value(), &self.gate_phrase(gate))
            }),
        }
    }

    /// The limits the worker holds this removal to: on each disk past the
    /// window's limit, the share the person confirmed for that disk, so a
    /// share that grew since is refused; on every other disk, the window's.
    /// Err with the notice to show.
    pub fn removal_limit(
        &self,
        plan: &Plan,
        cx: &App,
    ) -> Result<Ceilings, String> {
        let policy = self.reload_policy()?;
        // Off still counts against a capacity, so an unknown one fails
        // closed; it just never says no for size.
        let base = policy.limit.map_or_else(|| Limit::new(100), Ok)?;
        let (usage, names) = self.usage(plan, true)?;
        let gate = policy
            .limit
            .filter(|_| self.remote.is_none())
            .and_then(|limit| Gate::new(usage, &names, limit));
        let ceilings = match gate {
            Some(gate) if !self.gate_met(&gate, cx) => {
                return Err(match policy.confirm {
                    Confirm::Tick => format!(
                        "this removes {}; tick the box to go past the {}% \
                         limit",
                        gate.summary(),
                        gate.limit
                    ),
                    _ => format!(
                        "this removes {}; type \u{201c}{}\u{201d} to go past \
                         the {}% limit",
                        gate.summary(),
                        self.gate_phrase(&gate),
                        gate.limit
                    ),
                });
            }
            Some(gate) => gate.ceilings(base)?,
            None => Ceilings::from(base),
        };
        ceilings.check_with(&plan.targets, |path| {
            (self.disk_probe)(path).map(|disk| (disk.key, disk.capacity))
        })?;
        Ok(ceilings)
    }

    /// Make Review's phrase field once there is a window to make it in.
    /// Enter in it commits, as Enter on the list does, once it matches.
    pub fn prepare_over_phrase(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.over_phrase.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.commit(false, cx);
            }
            cx.notify();
        })
        .detach();
        self.over_phrase = Some(input);
    }

    /// Whether the phrase field has the keys: Review's own keys, `?`
    /// included, are letters typed into it then.
    pub fn editing_phrase(&self, window: &Window, cx: &App) -> bool {
        self.over_phrase.as_ref().is_some_and(|input| {
            input.read(cx).focus_handle(cx).is_focused(window)
        })
    }

    /// Show `notice` as a warning.
    pub fn warn(&mut self, notice: String, cx: &mut Context<'_, Self>) {
        self.say(notice, Status::Warning, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_is_over_exactly_where_the_worker_refuses() {
        let five = Limit::default();
        // 5% of 1000 is 50: 50 passes, 51 does not, as `Limit::check`.
        assert_eq!(Share::over(50, 1000, five), None);
        let over = Share::over(51, 1000, five).expect("over");
        assert_eq!((over.percent, over.limit), (6, 5));
        assert_eq!(
            Share::over(u64::MAX, 1, five).map(|s| s.percent),
            Some(100)
        );
    }

    fn usage(volume: &str, bytes: u64, capacity: u64) -> Usage {
        Usage {
            volume: volume.into(),
            bytes,
            capacity,
        }
    }

    #[test]
    fn a_gate_lists_each_disk_past_the_limit_and_raises_only_those() {
        let names = BTreeMap::from([
            ("boot".to_owned(), "/".to_owned()),
            ("usb".to_owned(), "/Volumes/USB".to_owned()),
        ]);
        let five = Limit::default();
        let gate = Gate::new(
            vec![
                usage("boot", 190, 1000),
                usage("other", 50, 1000),
                usage("usb", 200, 1000),
            ],
            &names,
            five,
        )
        .expect("two disks past 5%");
        // Largest first.
        assert_eq!(gate.percents(), [20, 19]);
        assert_eq!(gate.summary(), "20% of /Volumes/USB and 19% of /");
        assert_eq!(
            gate.ticked(RemovalMode::Trash),
            (
                vec![("usb".to_owned(), 20), ("boot".to_owned(), 19)],
                RemovalMode::Trash
            )
        );
        let ceilings = gate.ceilings(five).expect("ceilings");
        assert_eq!(ceilings.of("usb").percent(), 20);
        assert_eq!(ceilings.of("boot").percent(), 19);
        assert_eq!(ceilings.of("other"), five, "within the limit, held to it");

        assert_eq!(
            Gate::new(vec![usage("boot", 50, 1000)], &names, five),
            None
        );
        // A disk with no name reads as its key.
        let unnamed = Gate::new(vec![usage("disk9", 60, 1000)], &names, five)
            .expect("past 5%");
        assert_eq!(unnamed.summary(), "6% of disk9");
    }
}
