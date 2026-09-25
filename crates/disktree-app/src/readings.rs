//! Readings of what items are beyond their size, kept until the next scan.
//!
//! One can take seconds and several cores, so only a few run at once, and
//! what is pointed at goes first. This is only the bookkeeping: the window
//! starts what [`Readings::next`] hands it and reports back through
//! [`Readings::finish`].

use std::collections::VecDeque;
use std::hash::Hash;

use disktree_core::checkout::Reading;
use rustc_hash::{FxHashMap, FxHashSet};

pub struct Readings<Key, Input, Value> {
    values: FxHashMap<Key, Reading<Value>>,
    queue: VecDeque<(Key, Input)>,
    running: FxHashSet<Key>,
    /// Reads still running from before a [`Self::forget`] hold their place
    /// too.
    busy: usize,
    generation: u64,
    limit: usize,
}

impl<Key: Clone + Eq + Hash, Input, Value> Readings<Key, Input, Value> {
    pub fn new(limit: usize) -> Self {
        Self {
            values: FxHashMap::default(),
            queue: VecDeque::new(),
            running: FxHashSet::default(),
            busy: 0,
            generation: 0,
            limit,
        }
    }

    pub fn get(&self, key: &Key) -> Option<&Reading<Value>> {
        self.values.get(key)
    }

    /// Queue a reading unless there is one or it is under way. An urgent
    /// one goes to the front, even when it was already queued.
    pub fn request(&mut self, key: Key, input: Input, urgent: bool) {
        if self.values.contains_key(&key) || self.running.contains(&key) {
            return;
        }
        if let Some(queued) =
            self.queue.iter().position(|(known, _)| *known == key)
        {
            if !urgent {
                return;
            }
            self.queue.remove(queued);
        }
        if urgent {
            self.queue.push_front((key, input));
        } else {
            self.queue.push_back((key, input));
        }
    }

    /// Drop what is queued but no longer wanted; what is running finishes.
    pub fn retain(&mut self, wanted: impl Fn(&Key) -> bool) {
        self.queue.retain(|(key, _)| wanted(key));
    }

    /// The next reading to start, with the generation to finish it under,
    /// while fewer than the limit run.
    pub fn next(&mut self) -> Option<(Key, Input, u64)> {
        if self.busy >= self.limit {
            return None;
        }
        let (key, input) = self.queue.pop_front()?;
        self.running.insert(key.clone());
        self.busy += 1;
        Some((key, input, self.generation))
    }

    /// A reading from before the last [`Self::forget`] is dropped.
    pub fn finish(&mut self, key: Key, value: Reading<Value>, generation: u64) {
        self.busy = self.busy.saturating_sub(1);
        if generation == self.generation {
            self.running.remove(&key);
            self.values.insert(key, value);
        }
    }

    pub fn forget(&mut self) {
        self.generation += 1;
        self.queue.clear();
        self.running.clear();
        self.values.clear();
    }

    /// Read `key` again the next time it is wanted.
    pub fn forget_key(&mut self, key: &Key) {
        self.values.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(
        readings: &mut Readings<&'static str, &'static str, String>,
    ) -> Vec<&'static str> {
        let mut order = Vec::new();
        while let Some((key, input, generation)) = readings.next() {
            order.push(key);
            readings.finish(
                key,
                Reading::Done(input.to_uppercase()),
                generation,
            );
        }
        order
    }

    #[test]
    fn a_few_run_at_once_and_what_is_pointed_at_goes_first() {
        let mut readings = Readings::new(2);
        for key in ["a", "b", "c"] {
            readings.request(key, key, false);
        }
        let started: Vec<_> = std::iter::from_fn(|| readings.next()).collect();
        assert_eq!(
            started.iter().map(|(key, ..)| *key).collect::<Vec<_>>(),
            ["a", "b"],
            "two at a time"
        );
        readings.request("d", "d", true);
        readings.request("c", "c", false);
        readings.request("e", "e", false);
        readings.retain(|key| *key != "e");
        for (key, input, generation) in started {
            readings.finish(
                key,
                Reading::Done(input.to_uppercase()),
                generation,
            );
        }
        assert_eq!(
            drain(&mut readings),
            ["d", "c"],
            "the urgent one jumps the queue; the repeat and the withdrawn one \
             never run"
        );
        assert_eq!(readings.get(&"d"), Some(&Reading::Done("D".into())));
        assert_eq!(readings.get(&"e"), None);
        readings.request("d", "d", true);
        assert!(readings.next().is_none(), "a reading is kept");
    }

    #[test]
    fn forgetting_drops_a_read_still_running() {
        let mut readings: Readings<&str, &str, String> = Readings::new(2);
        readings.request("a", "a", false);
        let (key, input, generation) = readings.next().expect("started");
        readings.forget();
        readings.finish(key, Reading::Done(input.into()), generation);
        assert_eq!(readings.get(&"a"), None, "a scan landed since it started");
        readings.request("a", "a", false);
        assert_eq!(drain(&mut readings), ["a"]);
        assert_eq!(readings.get(&"a"), Some(&Reading::Done("A".into())));
    }
}
