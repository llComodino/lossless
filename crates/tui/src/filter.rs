use std::cmp::Reverse;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

/// fzf-style ranking, reusing its matcher and buffer across keystrokes.
pub struct Ranker {
    matcher: Matcher,
    buf: Vec<char>,
}

impl Ranker {
    pub fn new() -> Ranker {
        Ranker {
            matcher: Matcher::default(),
            buf: Vec::new(),
        }
    }

    /// Indices of the `lines` that match `query`, best first, ties in input
    /// order. `query` must not be empty: an empty pattern matches everything.
    pub fn rank<S: AsRef<str>>(
        &mut self,
        query: &str,
        lines: impl IntoIterator<Item = S>,
    ) -> Vec<usize> {
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut scored: Vec<(usize, u32)> = lines
            .into_iter()
            .enumerate()
            .filter_map(|(i, line)| {
                let hay = Utf32Str::new(line.as_ref(), &mut self.buf);
                pattern.score(hay, &mut self.matcher).map(|s| (i, s))
            })
            .collect();
        scored.sort_by_key(|&(_, s)| Reverse(s));
        scored.into_iter().map(|(i, _)| i).collect()
    }
}

/// One view's filter. `hits` holds real indices in rank order, `None`
/// while the query is empty (the whole list shows).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub query: String,
    pub editing: bool,
    pub hits: Option<Vec<usize>>,
}

impl Filter {
    /// Visible rows of a list of `total`.
    pub fn len(&self, total: usize) -> usize {
        self.hits.as_ref().map_or(total, Vec::len)
    }

    /// The real index shown at visible `pos`, `None` past the end.
    pub fn real(&self, pos: usize, total: usize) -> Option<usize> {
        match &self.hits {
            Some(h) => h.get(pos).copied().filter(|&i| i < total),
            None => (pos < total).then_some(pos),
        }
    }

    /// `items` as shown: `(real index, item)` in filter order.
    pub fn visible<'a, T>(&self, items: &'a [T]) -> Vec<(usize, &'a T)> {
        match &self.hits {
            Some(h) => h.iter().filter_map(|&i| Some((i, items.get(i)?))).collect(),
            None => items.iter().enumerate().collect(),
        }
    }

    /// Where real index `real` shows; unbounded without a filter, so callers
    /// clamp to the list.
    pub fn position(&self, real: usize) -> Option<usize> {
        match &self.hits {
            Some(h) => h.iter().position(|&r| r == real),
            None => Some(real),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const WINNER: &str = "ABBA - The Winner Takes It All (Super Trouper)";
    const FERNANDO: &str = "ABBA - Fernando (Arrival)";

    #[test]
    fn subsequence_matches() {
        let mut r = Ranker::new();
        assert_eq!(r.rank("wtta", [FERNANDO, WINNER]), vec![1]);
    }

    #[test]
    fn best_first() {
        let mut r = Ranker::new();
        let lines = ["Other - Song (abba hits)", "ABBA - Waterloo (Waterloo)"];
        assert_eq!(r.rank("abba", lines), vec![1, 0]);
    }

    #[test]
    fn ties_keep_input_order() {
        let mut r = Ranker::new();
        assert_eq!(
            r.rank("abba", ["ABBA - A", "ABBA - A", "ABBA - A"]),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn smart_case() {
        let mut r = Ranker::new();
        assert_eq!(r.rank("bjork", ["Björk - x"]), vec![0]);
        assert!(r.rank("Bjork", ["bjork - x"]).is_empty());
    }

    proptest! {
        #[test]
        fn non_ascii_queries_do_not_panic(q in any::<String>(), lines in proptest::collection::vec(any::<String>(), 0..8)) {
            let mut r = Ranker::new();
            let hits = r.rank(&q, &lines);
            prop_assert!(hits.iter().all(|&i| i < lines.len()));
        }
    }

    #[test]
    fn real_and_position_round_trip() {
        let none = Filter::default();
        assert_eq!(none.len(5), 5);
        assert_eq!(none.real(3, 5), Some(3));
        assert_eq!(none.real(5, 5), None);
        assert_eq!(none.position(4), Some(4));
        let f = Filter {
            query: "x".into(),
            editing: false,
            hits: Some(vec![4, 1, 3]),
        };
        assert_eq!(f.len(5), 3);
        assert_eq!(f.real(0, 5), Some(4));
        assert_eq!(f.real(3, 5), None);
        assert_eq!(f.real(0, 4), None, "a hit past a shrunk list");
        assert_eq!(f.position(3), Some(2));
        assert_eq!(f.position(0), None);
        for p in 0..3 {
            assert_eq!(f.position(f.real(p, 5).unwrap()), Some(p));
        }
        let items = ["a", "b", "c", "d", "e"];
        assert_eq!(f.visible(&items), [(4, &"e"), (1, &"b"), (3, &"d")]);
        assert_eq!(none.visible(&items[..2]), [(0, &"a"), (1, &"b")]);
        assert_eq!(f.visible(&items[..2]), [(1, &"b")]);
    }
}
