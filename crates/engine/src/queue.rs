use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;

use lossless_proto::Repeat;

use crate::error::{Error, Result};

#[derive(Debug)]
pub(crate) struct Queue {
    entries: Vec<PathBuf>,
    current: Option<usize>,
    repeat: Repeat,
    random: bool,
    order: Vec<usize>,
    history: Vec<usize>,
    cycle_start: usize,
    seed: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self::with_seed(RandomState::new().build_hasher().finish())
    }
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn shuffle(items: &mut [usize], seed: &mut u64) {
    for i in (1..items.len()).rev() {
        let j = (xorshift(seed) % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

struct Step {
    index: usize,
    new_cycle: Option<Vec<usize>>,
    from_order: bool,
    seed: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    Kept,
    CurrentRemoved { next: Option<usize> },
}

impl Queue {
    pub(crate) fn with_seed(seed: u64) -> Self {
        Self {
            entries: Vec::new(),
            current: None,
            repeat: Repeat::Off,
            random: false,
            order: Vec::new(),
            history: Vec::new(),
            cycle_start: 0,
            seed: seed | 1,
        }
    }

    pub(crate) fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    pub(crate) fn current(&self) -> Option<usize> {
        self.current
    }

    pub(crate) fn set_current(&mut self, i: Option<usize>) {
        if let Some(old) = self.current.filter(|&o| i.is_some_and(|n| n != o)) {
            self.push_history(old);
        }
        self.current = i;
        self.order.retain(|&o| Some(o) != i);
    }

    fn push_history(&mut self, i: usize) {
        self.history.push(i);
        self.trim_history();
    }

    fn trim_history(&mut self) {
        let cut = self.history.len().saturating_sub(self.entries.len());
        let dropped: Vec<usize> = self.history.drain(..cut).collect();
        let kept = dropped.len().saturating_sub(self.cycle_start);
        self.cycle_start = self.cycle_start.saturating_sub(cut);
        for &d in &dropped[dropped.len() - kept..] {
            if Some(d) != self.current
                && !self.order.contains(&d)
                && !self.history[self.cycle_start..].contains(&d)
            {
                self.order.insert(0, d);
            }
        }
    }

    fn rebuild_order(&mut self) {
        self.order = (0..self.entries.len())
            .filter(|&i| Some(i) != self.current && !self.history[self.cycle_start..].contains(&i))
            .collect();
        shuffle(&mut self.order, &mut self.seed);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_mode(&mut self, repeat: Option<Repeat>, random: Option<bool>) {
        if let Some(r) = repeat {
            self.repeat = r;
        }
        if let Some(on) = random {
            let was = std::mem::replace(&mut self.random, on);
            if on && !was {
                self.rebuild_order();
            }
        }
    }

    fn plan(&self) -> Option<Step> {
        let len = self.entries.len();
        if len == 0 {
            return None;
        }
        let step = |index| Step {
            index,
            new_cycle: None,
            from_order: false,
            seed: self.seed,
        };
        if let (Repeat::One, Some(c)) = (self.repeat, self.current) {
            return Some(step(c));
        }
        if !self.random {
            return match self.current {
                None => Some(step(0)),
                Some(c) if c + 1 < len => Some(step(c + 1)),
                Some(_) if self.repeat == Repeat::Queue => Some(step(0)),
                Some(_) => None,
            };
        }
        if let Some(&i) = self.order.last() {
            return Some(Step {
                from_order: true,
                ..step(i)
            });
        }
        if self.repeat != Repeat::Queue {
            return None;
        }
        let mut seed = self.seed;
        let mut pool: Vec<usize> = (0..len).collect();
        shuffle(&mut pool, &mut seed);
        if len > 1 && pool.last().copied() == self.current {
            pool.swap(0, len - 1);
        }
        let index = pool.pop()?;
        Some(Step {
            index,
            new_cycle: Some(pool),
            from_order: false,
            seed,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn peek_next(&self) -> Option<usize> {
        self.plan().map(|s| s.index)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn next(&mut self) -> Option<usize> {
        let step = self.plan()?;
        self.seed = step.seed;
        let old = self.current.replace(step.index);
        match step.new_cycle {
            Some(order) => {
                self.order = order;
                if let Some(old) = old {
                    self.push_history(old);
                }
                self.cycle_start = self.history.len();
            }
            None => {
                if step.from_order {
                    self.order.pop();
                }
                if let Some(old) = old.filter(|&c| c != step.index) {
                    self.push_history(old);
                }
            }
        }
        Some(step.index)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn previous(&mut self) -> Option<usize> {
        let target = loop {
            match self.history.pop() {
                Some(h) if Some(h) == self.current => continue,
                Some(h) => break Some(h),
                None => break self.current.and_then(|c| c.checked_sub(1)),
            }
        };
        self.cycle_start = self.cycle_start.min(self.history.len());
        if let Some(t) = target {
            let left = self.current.replace(t).filter(|&l| l != t);
            self.order.retain(|&o| o != t && Some(o) != left);
            if let Some(l) = left {
                self.order.insert(0, l);
            }
        }
        target
    }

    fn remap_history(&mut self, f: impl Fn(usize) -> Option<usize>) {
        let prefix = self.history[..self.cycle_start]
            .iter()
            .filter(|&&h| f(h).is_some())
            .count();
        self.history = self.history.iter().filter_map(|&h| f(h)).collect();
        self.cycle_start = prefix;
        self.trim_history();
        self.rebuild_order();
    }

    pub(crate) fn insert(&mut self, at: Option<usize>, paths: Vec<PathBuf>) -> Result<()> {
        let len = self.entries.len();
        let at = at.unwrap_or(len);
        if at > len {
            return Err(Error::OutOfRange { index: at, len });
        }
        let n = paths.len();
        self.entries.splice(at..at, paths);
        if let Some(c) = self.current.as_mut().filter(|c| at <= **c) {
            *c += n;
        }
        self.remap_history(|h| Some(if at <= h { h + n } else { h }));
        Ok(())
    }

    pub(crate) fn remove(&mut self, indices: &[usize]) -> Result<Removal> {
        let len = self.entries.len();
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        if let Some(w) = sorted.windows(2).find(|w| w[0] == w[1]) {
            return Err(Error::BadRequest(format!("duplicate index {}", w[0])));
        }
        if let Some(&index) = sorted.last().filter(|&&i| i >= len) {
            return Err(Error::OutOfRange { index, len });
        }
        let removed_current = self
            .current
            .is_some_and(|c| sorted.binary_search(&c).is_ok());
        let before = |i: usize| sorted.partition_point(|&r| r < i);
        let outcome = match self.current {
            Some(c) if removed_current => {
                let next = (c + 1..len).find(|i| sorted.binary_search(i).is_err());
                self.current = None;
                Removal::CurrentRemoved {
                    next: next.map(|n| n - before(n)),
                }
            }
            Some(c) => {
                self.current = Some(c - before(c));
                Removal::Kept
            }
            None => Removal::Kept,
        };
        let mut i = 0;
        self.entries.retain(|_| {
            let keep = sorted.binary_search(&i).is_err();
            i += 1;
            keep
        });
        self.remap_history(|h| sorted.binary_search(&h).is_err().then(|| h - before(h)));
        Ok(outcome)
    }

    pub(crate) fn move_entry(&mut self, from: usize, to: usize) -> Result<()> {
        let len = self.entries.len();
        if let Some(index) = [from, to].into_iter().find(|&i| i >= len) {
            return Err(Error::OutOfRange { index, len });
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        let moved = |c: usize| {
            if c == from {
                to
            } else if from < c && c <= to {
                c - 1
            } else if to <= c && c < from {
                c + 1
            } else {
                c
            }
        };
        self.current = self.current.map(moved);
        self.remap_history(|h| Some(moved(h)));
        Ok(())
    }

    pub(crate) fn replace(&mut self, paths: Vec<PathBuf>) {
        self.entries = paths;
        self.current = None;
        self.history.clear();
        self.cycle_start = 0;
        self.rebuild_order();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use lossless_proto::Repeat;

    use super::{Queue, Removal};
    use crate::Error;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn queue(names: &[&str], current: Option<usize>) -> Queue {
        let mut q = Queue::default();
        q.replace(paths(names));
        q.set_current(current);
        q
    }

    #[test]
    fn insert_at_start_shifts_current() {
        let mut q = queue(&["a", "b"], Some(1));
        q.insert(Some(0), paths(&["x", "y"])).unwrap();
        assert_eq!(q.entries(), paths(&["x", "y", "a", "b"]));
        assert_eq!(q.current(), Some(3));
        q.insert(None, paths(&["z"])).unwrap();
        q.insert(Some(5), vec![]).unwrap();
        assert_eq!(q.entries().len(), 5);
        assert_eq!(q.current(), Some(3));
    }

    #[test]
    fn insert_past_end_is_out_of_range() {
        let mut q = queue(&["a"], None);
        let e = q.insert(Some(2), paths(&["x"])).unwrap_err();
        assert!(matches!(e, Error::OutOfRange { index: 2, len: 1 }));
        assert_eq!(q.entries().len(), 1);
    }

    #[test]
    fn remove_current_reports_the_next() {
        let mut q = queue(&["a", "b", "c", "d"], Some(1));
        assert_eq!(
            q.remove(&[0, 1]).unwrap(),
            Removal::CurrentRemoved { next: Some(0) }
        );
        assert_eq!(q.entries(), paths(&["c", "d"]));
        assert_eq!(q.current(), None);
        let mut q = queue(&["a", "b"], Some(1));
        assert_eq!(
            q.remove(&[1]).unwrap(),
            Removal::CurrentRemoved { next: None }
        );
        let mut q = queue(&["a", "b", "c"], Some(2));
        assert_eq!(q.remove(&[0]).unwrap(), Removal::Kept);
        assert_eq!(q.current(), Some(1));
        assert_eq!(q.remove(&[]).unwrap(), Removal::Kept);
    }

    #[test]
    fn remove_rejects_duplicates_and_out_of_range_atomically() {
        let mut q = queue(&["a", "b"], Some(0));
        let e = q.remove(&[1, 1]).unwrap_err();
        assert!(matches!(&e, Error::BadRequest(m) if m == "duplicate index 1"));
        let e = q.remove(&[0, 2]).unwrap_err();
        assert!(matches!(e, Error::OutOfRange { index: 2, len: 2 }));
        assert_eq!(q.entries(), paths(&["a", "b"]));
        assert_eq!(q.current(), Some(0));
    }

    #[test]
    fn move_keeps_current_on_its_file() {
        let mut q = queue(&["a", "b", "c", "d"], Some(1));
        q.move_entry(1, 3).unwrap();
        assert_eq!((q.entries()[3].to_str(), q.current()), (Some("b"), Some(3)));
        q.move_entry(0, 3).unwrap();
        assert_eq!(q.entries()[q.current().unwrap()], PathBuf::from("b"));
        q.move_entry(3, 0).unwrap();
        assert_eq!(q.entries()[q.current().unwrap()], PathBuf::from("b"));
        assert!(matches!(
            q.move_entry(0, 4),
            Err(Error::OutOfRange { index: 4, len: 4 })
        ));
        assert!(matches!(
            q.move_entry(9, 0),
            Err(Error::OutOfRange { index: 9, len: 4 })
        ));
    }

    #[derive(Debug, Clone)]
    enum Edit {
        Insert(Option<usize>, usize),
        Remove(Vec<usize>),
        Move(usize, usize),
    }

    fn edit() -> impl Strategy<Value = Edit> {
        prop_oneof![
            (proptest::option::of(0..12usize), 0..4usize).prop_map(|(a, n)| Edit::Insert(a, n)),
            proptest::collection::vec(0..12usize, 0..4).prop_map(Edit::Remove),
            (0..12usize, 0..12usize).prop_map(|(a, b)| Edit::Move(a, b)),
        ]
    }

    proptest! {
        #[test]
        fn edits_keep_current_on_the_same_file(
            start in 0..8usize,
            cur in proptest::option::of(0..8usize),
            edits in proptest::collection::vec(edit(), 0..30),
        ) {
            let mut next_id = start;
            let names: Vec<String> = (0..start).map(|i| format!("f{i}")).collect();
            let mut q = Queue::default();
            q.replace(names.iter().map(PathBuf::from).collect());
            q.set_current(cur.filter(|&c| c < start));
            for e in edits {
                let before = q.current().map(|c| q.entries()[c].clone());
                let removed: Vec<PathBuf> = match &e {
                    Edit::Remove(ix) => ix.iter().filter_map(|&i| q.entries().get(i).cloned()).collect(),
                    _ => vec![],
                };
                let ok = match e {
                    Edit::Insert(at, n) => {
                        let new = (0..n).map(|_| { next_id += 1; PathBuf::from(format!("f{next_id}")) }).collect();
                        q.insert(at, new).is_ok()
                    }
                    Edit::Remove(ix) => q.remove(&ix).is_ok(),
                    Edit::Move(a, b) => q.move_entry(a, b).is_ok(),
                };
                if let Some(c) = q.current() {
                    prop_assert!(c < q.entries().len());
                }
                if let (true, Some(p)) = (ok, before) {
                    if !removed.contains(&p) {
                        prop_assert_eq!(&q.entries()[q.current().unwrap()], &p);
                    } else {
                        prop_assert_eq!(q.current(), None);
                    }
                }
            }
        }
    }

    fn seeded(n: usize, repeat: Repeat, random: bool) -> Queue {
        let mut q = Queue::with_seed(0x9e37_79b9_7f4a_7c15);
        q.replace((0..n).map(|i| PathBuf::from(format!("f{i}"))).collect());
        q.set_mode(Some(repeat), Some(random));
        q
    }

    fn take(q: &mut Queue, n: usize) -> Vec<usize> {
        (0..n).map(|_| q.next().unwrap()).collect()
    }

    fn sorted(mut v: Vec<usize>) -> Vec<usize> {
        v.sort_unstable();
        v
    }

    #[test]
    fn repeat_one_repeats() {
        let mut q = seeded(3, Repeat::One, true);
        q.set_current(Some(1));
        assert_eq!(take(&mut q, 4), [1, 1, 1, 1]);
        q.set_mode(None, Some(false));
        assert_eq!(q.next(), Some(1));
    }

    #[test]
    fn repeat_queue_wraps() {
        let mut q = seeded(3, Repeat::Queue, false);
        q.set_current(Some(1));
        assert_eq!(take(&mut q, 4), [2, 0, 1, 2]);
    }

    #[test]
    fn off_stops_at_the_end() {
        let mut q = seeded(3, Repeat::Off, false);
        assert_eq!(take(&mut q, 3), [0, 1, 2]);
        assert_eq!(q.next(), None);
        assert_eq!(q.current(), Some(2));
    }

    #[test]
    fn random_plays_each_entry_once_per_cycle() {
        let all: Vec<usize> = (0..100).collect();
        let mut q = seeded(100, Repeat::Queue, true);
        let first = take(&mut q, 100);
        assert_eq!(sorted(first.clone()), all);
        let second = take(&mut q, 100);
        assert_eq!(sorted(second.clone()), all);
        assert_ne!(first, second);
        assert_ne!(first, all);
        let mut q = seeded(100, Repeat::Off, true);
        assert_eq!(sorted(take(&mut q, 100)), all);
        assert_eq!(q.next(), None);
    }

    #[test]
    fn previous_follows_history() {
        let mut q = seeded(5, Repeat::Off, false);
        assert_eq!(take(&mut q, 3), [0, 1, 2]);
        assert_eq!(q.previous(), Some(1));
        assert_eq!(q.previous(), Some(0));
        assert_eq!(q.previous(), None);
        let mut q = seeded(5, Repeat::Off, false);
        q.set_current(Some(3));
        assert_eq!(q.previous(), Some(2));
        q.set_current(Some(4));
        assert_eq!(q.previous(), Some(2));
        let mut q = seeded(5, Repeat::Off, true);
        let played = take(&mut q, 3);
        assert_eq!(q.previous(), Some(played[1]));
        assert_eq!(q.previous(), Some(played[0]));
    }

    #[test]
    fn random_handles_tiny_queues() {
        let mut q = seeded(0, Repeat::Queue, true);
        assert_eq!(q.next(), None);
        let mut q = seeded(1, Repeat::Off, true);
        assert_eq!(q.next(), Some(0));
        assert_eq!(q.next(), None);
        let mut q = seeded(1, Repeat::Queue, true);
        assert_eq!(take(&mut q, 50), vec![0; 50]);
        let mut q = seeded(2, Repeat::Queue, true);
        let seen = take(&mut q, 1000);
        assert!(seen.contains(&0) && seen.contains(&1));
        assert!(seen.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn peek_matches_next() {
        for (repeat, random) in [
            (Repeat::Off, false),
            (Repeat::Queue, false),
            (Repeat::One, false),
            (Repeat::Off, true),
            (Repeat::Queue, true),
            (Repeat::One, true),
        ] {
            let mut q = seeded(4, repeat, random);
            for _ in 0..20 {
                let peeked = q.peek_next();
                assert_eq!(q.peek_next(), peeked);
                assert_eq!(q.next(), peeked);
            }
        }
    }

    #[test]
    fn edits_remap_history() {
        let mut q = seeded(5, Repeat::Off, false);
        take(&mut q, 4);
        q.remove(&[0]).unwrap();
        assert_eq!(q.previous(), Some(1));
        q.insert(Some(0), paths(&["x"])).unwrap();
        assert_eq!(q.previous(), Some(1));
    }

    #[test]
    fn previous_works_right_after_a_wrap() {
        for seed in 1..40 {
            let mut q = seeded(3, Repeat::Queue, true);
            q.seed = seed;
            q.rebuild_order();
            let played = take(&mut q, 4);
            assert_eq!(q.previous(), Some(played[2]), "seed {seed}");
        }
    }

    #[test]
    fn repeat_one_with_no_current_leaves_the_order_clean() {
        let mut q = seeded(4, Repeat::One, true);
        let first = q.next().unwrap();
        assert_eq!(q.current(), Some(first));
        assert!(!q.order.contains(&first));
        q.set_mode(Some(Repeat::Off), None);
        let rest = take(&mut q, 3);
        assert!(!rest.contains(&first));
        assert_eq!(q.next(), None);
    }

    #[test]
    fn edits_remap_history_precisely() {
        let mut q = seeded(6, Repeat::Off, false);
        take(&mut q, 5);
        assert_eq!(q.history, [0, 1, 2, 3]);
        q.remove(&[1, 3]).unwrap();
        assert_eq!(q.history, [0, 1]);
        q.move_entry(0, 2).unwrap();
        assert_eq!(q.history, [2, 0]);
        q.insert(Some(1), paths(&["x", "y"])).unwrap();
        assert_eq!(q.history, [4, 0]);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Edit(Edit),
        Jump(usize),
        Prev,
        Next,
        Mode(u8, bool),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => edit().prop_map(Op::Edit),
            2 => (0..12usize).prop_map(Op::Jump),
            2 => Just(Op::Prev),
            4 => Just(Op::Next),
            2 => (0..3u8, any::<bool>()).prop_map(|(r, b)| Op::Mode(r, b)),
        ]
    }

    fn history_paths(q: &Queue) -> Vec<PathBuf> {
        q.history.iter().map(|&h| q.entries()[h].clone()).collect()
    }

    proptest! {
        #[test]
        fn edits_rebuild_the_order(
            start in 0..8usize,
            ops in proptest::collection::vec(op(), 0..40),
        ) {
            let mut next_id = start;
            let mut q = seeded(start, Repeat::Queue, true);
            for o in ops {
                match o {
                    Op::Edit(e) => {
                        let before = history_paths(&q);
                        let removed: Vec<PathBuf> = match &e {
                            Edit::Remove(ix) => ix.iter().filter_map(|&i| q.entries().get(i).cloned()).collect(),
                            _ => vec![],
                        };
                        let ok = match e {
                            Edit::Insert(at, n) => {
                                let new = (0..n).map(|_| { next_id += 1; PathBuf::from(format!("f{next_id}")) }).collect();
                                q.insert(at, new).is_ok()
                            }
                            Edit::Remove(ix) => q.remove(&ix).is_ok(),
                            Edit::Move(a, b) => q.move_entry(a, b).is_ok(),
                        };
                        if ok {
                            let mut want: Vec<PathBuf> = before.into_iter().filter(|p| !removed.contains(p)).collect();
                            let cut = want.len().saturating_sub(q.entries().len());
                            want.drain(..cut);
                            prop_assert_eq!(history_paths(&q), want);
                        }
                    }
                    Op::Jump(i) => {
                        if i < q.entries().len() {
                            q.set_current(Some(i));
                        }
                    }
                    Op::Prev => {
                        q.previous();
                    }
                    Op::Next => {
                        q.next();
                    }
                    Op::Mode(r, b) => {
                        let r = [Repeat::Off, Repeat::Queue, Repeat::One][r as usize];
                        q.set_mode(Some(r), Some(b));
                    }
                }
                let len = q.entries().len();
                prop_assert!(!q.random || q.order.iter().all(|&o| o < len && Some(o) != q.current()));
                prop_assert!(q.history.iter().all(|&h| h < len));
                prop_assert!(q.history.len() <= len);
                let mut sorted_order = q.order.clone();
                sorted_order.sort_unstable();
                sorted_order.dedup();
                prop_assert_eq!(sorted_order.len(), q.order.len());
                if let Some(c) = q.current() {
                    prop_assert!(c < len);
                }
                if q.random {
                    for i in 0..len {
                        prop_assert!(
                            q.order.contains(&i) || q.history.contains(&i) || q.current() == Some(i),
                            "entry {} lost", i
                        );
                    }
                }
            }
        }
    }
}
