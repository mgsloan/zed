use std::fmt::Debug;
use std::{array, collections::VecDeque};
use util::debug_panic;

use crate::{HashFrom, Occurrences};

#[derive(Debug)]
pub struct SlidingWindow<const TN: usize, T, D, S> {
    targets: [T; TN],
    intersection: Occurrences<S>,
    regions: VecDeque<WeightedOverlapRegion<TN, D, S>>,
    window_count: u32,
    numerators: [u32; TN],
    jaccard_denominator_parts: [u32; TN],
}

#[derive(Debug)]
struct WeightedOverlapRegion<const TN: usize, D, S> {
    data: D,
    added_hashes: Vec<AddedHash<TN, S>>,
    window_count_delta: u32,
}

#[derive(Debug)]
struct AddedHash<const TN: usize, S> {
    hash: HashFrom<S>,
    target_counts: [u32; TN],
}

impl<const TN: usize, T: AsRef<Occurrences<S>>, D, S> SlidingWindow<TN, T, D, S> {
    pub fn new(targets: [T; TN]) -> Self {
        Self::with_capacity(targets, 0)
    }

    pub fn with_capacity(targets: [T; TN], capacity: usize) -> Self {
        let jaccard_denominator_parts = targets.each_ref().map(|target| target.as_ref().len());
        Self {
            targets,
            intersection: Occurrences::default().into(),
            regions: VecDeque::with_capacity(capacity),
            window_count: 0,
            numerators: [0; TN],
            jaccard_denominator_parts: jaccard_denominator_parts,
        }
    }

    pub fn clear(&mut self) {
        self.intersection.clear();
        self.regions.clear();
        self.window_count = 0;
        self.numerators = [0; TN];
        self.jaccard_denominator_parts =
            self.targets.each_ref().map(|target| target.as_ref().len());
    }

    pub fn push_back(&mut self, data: D, hashes: impl IntoIterator<Item = HashFrom<S>>) {
        let mut added_hashes = Vec::new();
        let mut window_count_delta = 0;
        for hash in hashes {
            window_count_delta += 1;
            let target_counts =
                array::from_fn(|target_ix| self.targets[target_ix].as_ref().get_count(hash));
            if target_counts.iter().any(|count| *count > 0) {
                added_hashes.push(AddedHash {
                    hash,
                    target_counts,
                });
                let window_hash_count = self.intersection.add_hash(hash);
                for (target_ix, target_count) in target_counts.iter().enumerate() {
                    if window_hash_count <= *target_count {
                        self.numerators[target_ix] += 1;
                    } else {
                        self.jaccard_denominator_parts[target_ix] += 1;
                    }
                }
            }
        }
        self.window_count += window_count_delta;
        self.regions.push_back(WeightedOverlapRegion {
            data,
            added_hashes,
            window_count_delta,
        });
    }

    pub fn pop_front(&mut self) -> D {
        let removed = self
            .regions
            .pop_front()
            .expect("No sliding window region to remove");

        for AddedHash {
            hash,
            target_counts,
        } in removed.added_hashes
        {
            let window_hash_count = self.intersection.remove_hash(hash);
            for (target_ix, target_count) in target_counts.iter().enumerate() {
                if window_hash_count < *target_count {
                    if let Some(numerator) = self.numerators[target_ix].checked_sub(1) {
                        self.numerators[target_ix] = numerator;
                    } else {
                        debug_panic!("bug: underflow in sliding window text similarity");
                    }
                } else {
                    if let Some(jaccard_denominator_part) =
                        self.jaccard_denominator_parts[target_ix].checked_sub(1)
                    {
                        self.jaccard_denominator_parts[target_ix] = jaccard_denominator_part;
                    } else {
                        debug_panic!("bug: underflow in sliding window text similarity");
                    }
                }
            }
        }

        if let Some(window_count) = self.window_count.checked_sub(removed.window_count_delta) {
            self.window_count = window_count;
        } else {
            debug_panic!("bug: underflow in sliding window text similarity");
        }

        removed.data
    }

    pub fn weighted_overlap_coefficient(&self) -> [f32; TN] {
        array::from_fn(|target_ix| {
            let denominator = self.targets[target_ix]
                .as_ref()
                .len()
                .min(self.window_count);
            if denominator == 0 {
                0.0
            } else {
                self.numerators[target_ix] as f32 / denominator as f32
            }
        })
    }

    pub fn weighted_jaccard_similarity(&self) -> [f32; TN] {
        array::from_fn(|target_ix| {
            let mut denominator = self.jaccard_denominator_parts[target_ix];
            if let Some(other_denominator_part) =
                self.window_count.checked_sub(self.intersection.len())
            {
                denominator += other_denominator_part;
            } else {
                debug_panic!("bug: underflow in sliding window text similarity");
            }
            self.numerator as f32 / denominator as f32
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{IdentifierParts, OccurrenceSource, Occurrences, WeightedSimilarity};

    #[test]
    fn test_sliding_window() {
        let target = Occurrences::new(IdentifierParts::occurrences_in_str("a b c d"));
        let mut checked_window = CheckedSlidingWindow::new(target);

        checked_window.push_back("a");
        checked_window.pop_front();

        checked_window.push_back("a b");
        checked_window.push_back("a");
        checked_window.pop_front();
        checked_window.pop_front();

        checked_window.push_back("a b");
        checked_window.push_back("a b c");
        checked_window.pop_front();
        checked_window.push_back("a b c d");
        checked_window.pop_front();
        checked_window.pop_front();
    }

    #[derive(Debug)]
    struct CheckedSlidingWindow {
        inner: SlidingWindow<u32, Occurrences<IdentifierParts>, 1, IdentifierParts>,
        text: String,
        first_line: u32,
        last_line: u32,
    }

    impl CheckedSlidingWindow {
        fn new(target: Occurrences<IdentifierParts>) -> Self {
            CheckedSlidingWindow {
                inner: SlidingWindow::new(target),
                text: String::new(),
                first_line: 0,
                last_line: 0,
            }
        }

        #[track_caller]
        fn push_back(&mut self, line: &str) {
            self.inner
                .push_back(self.last_line, IdentifierParts::occurrences_in_str(line));
            self.text.push_str(line);
            self.text.push('\n');
            self.last_line += 1;
            self.check_after_mutation();
        }

        #[track_caller]
        fn pop_front(&mut self) {
            assert_eq!(self.inner.pop_front(), self.first_line);
            self.text.drain(0..self.text.find("\n").unwrap() + 1);
            self.first_line += 1;
            self.check_after_mutation();
        }

        #[track_caller]
        fn check_after_mutation(&self) {
            assert_eq!(
                self.inner.weighted_overlap_coefficient(),
                Occurrences::new(IdentifierParts::occurrences_in_str(&self.text))
                    .weighted_overlap_coefficient(&self.inner.targets),
                "weighted_overlap_coefficient"
            );
            assert_eq!(
                self.inner.weighted_jaccard_similarity(),
                Occurrences::new(IdentifierParts::occurrences_in_str(&self.text))
                    .weighted_jaccard_similarity(&self.inner.targets),
                "weighted_jaccard_similarity"
            );
        }
    }
}
