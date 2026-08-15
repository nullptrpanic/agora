use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ByteRange {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

impl ByteRange {
    pub(crate) fn new(start: u64, end: u64) -> Result<Self> {
        if start >= end {
            bail!("invalid filesystem byte range");
        }
        Ok(Self { start, end })
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ByteRangeSet {
    ranges: Vec<ByteRange>,
    #[cfg(test)]
    insertion_visits: usize,
}

impl ByteRangeSet {
    pub(crate) fn insert(&mut self, mut range: ByteRange) {
        if range.start >= range.end {
            return;
        }
        if self.ranges.last().is_none_or(|last| last.end < range.start) {
            self.ranges.push(range);
            return;
        }

        let first = self
            .ranges
            .partition_point(|resident| resident.end < range.start);
        let mut end = first;
        while let Some(resident) = self.ranges.get(end)
            && resident.start <= range.end
        {
            #[cfg(test)]
            {
                self.insertion_visits += 1;
            }
            range.start = range.start.min(resident.start);
            range.end = range.end.max(resident.end);
            end += 1;
        }
        self.ranges.splice(first..end, std::iter::once(range));
    }

    pub(crate) fn missing(&self, range: ByteRange) -> Vec<ByteRange> {
        let mut missing = Vec::new();
        let mut start = range.start;
        let first = self
            .ranges
            .partition_point(|resident| resident.end <= range.start);
        for resident in &self.ranges[first..] {
            if resident.start >= range.end {
                break;
            }
            if resident.start > start {
                missing.push(ByteRange {
                    start,
                    end: resident.start.min(range.end),
                });
            }
            start = start.max(resident.end);
            if start >= range.end {
                break;
            }
        }
        if start < range.end {
            missing.push(ByteRange {
                start,
                end: range.end,
            });
        }
        missing
    }

    pub(crate) fn covers(&self, range: ByteRange) -> bool {
        let end = self
            .ranges
            .partition_point(|resident| resident.start <= range.start);
        end != 0 && self.ranges[end - 1].end >= range.end
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.ranges.clear();
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &ByteRange> {
        self.ranges.iter()
    }

    pub(crate) fn to_vec(&self) -> Vec<ByteRange> {
        self.ranges.clone()
    }

    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[ByteRange] {
        &self.ranges
    }

    #[cfg(test)]
    pub(crate) fn insertion_visits(&self) -> usize {
        self.insertion_visits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_set_merges_and_queries_ranges_without_rescanning_ordered_insertions() {
        let mut ranges = ByteRangeSet::default();
        ranges.insert(ByteRange { start: 3, end: 3 });
        ranges.insert(ByteRange { start: 3, end: 6 });
        ranges.insert(ByteRange { start: 1, end: 4 });

        assert_eq!(ranges.as_slice(), &[ByteRange { start: 1, end: 6 }]);
        assert!(ranges.covers(ByteRange { start: 2, end: 5 }));
        assert!(!ranges.covers(ByteRange { start: 0, end: 5 }));
        assert_eq!(
            ranges.missing(ByteRange { start: 0, end: 8 }),
            vec![
                ByteRange { start: 0, end: 1 },
                ByteRange { start: 6, end: 8 },
            ]
        );

        let mut disjoint = ByteRangeSet::default();
        for index in 0..512_u64 {
            disjoint.insert(ByteRange {
                start: index * 2,
                end: index * 2 + 1,
            });
        }
        assert_eq!(disjoint.as_slice().len(), 512);
        assert_eq!(disjoint.insertion_visits(), 0);
    }
}
