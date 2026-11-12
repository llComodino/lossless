use std::path::PathBuf;

use crate::error::{Error, Result};

#[derive(Debug, Default)]
pub(crate) struct Queue {
    entries: Vec<PathBuf>,
    current: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    Kept,
    CurrentRemoved { next: Option<usize> },
}

impl Queue {
    pub(crate) fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    pub(crate) fn current(&self) -> Option<usize> {
        self.current
    }

    pub(crate) fn set_current(&mut self, i: Option<usize>) {
        self.current = i;
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
        Ok(outcome)
    }

    pub(crate) fn move_entry(&mut self, from: usize, to: usize) -> Result<()> {
        let len = self.entries.len();
        if let Some(index) = [from, to].into_iter().find(|&i| i >= len) {
            return Err(Error::OutOfRange { index, len });
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        if let Some(c) = self.current {
            self.current = Some(if c == from {
                to
            } else if from < c && c <= to {
                c - 1
            } else if to <= c && c < from {
                c + 1
            } else {
                c
            });
        }
        Ok(())
    }

    pub(crate) fn replace(&mut self, paths: Vec<PathBuf>) {
        self.entries = paths;
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

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
}
