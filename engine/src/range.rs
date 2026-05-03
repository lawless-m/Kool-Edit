//! Sample-frame ranges. Half-open: `[start, end)`.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "SampleRangeRaw", into = "SampleRangeRaw")]
pub struct SampleRange {
    start: u64,
    end: u64,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
struct SampleRangeRaw {
    start: u64,
    end: u64,
}

impl TryFrom<SampleRangeRaw> for SampleRange {
    type Error = RangeError;
    fn try_from(raw: SampleRangeRaw) -> Result<Self, Self::Error> {
        SampleRange::new(raw.start, raw.end)
    }
}

impl From<SampleRange> for SampleRangeRaw {
    fn from(r: SampleRange) -> Self {
        Self {
            start: r.start,
            end: r.end,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RangeError {
    EndBeforeStart { start: u64, end: u64 },
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RangeError::EndBeforeStart { start, end } => {
                write!(f, "end {end} is before start {start}")
            }
        }
    }
}

impl std::error::Error for RangeError {}

impl SampleRange {
    pub fn new(start: u64, end: u64) -> Result<Self, RangeError> {
        if end < start {
            return Err(RangeError::EndBeforeStart { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn empty_at(point: u64) -> Self {
        Self {
            start: point,
            end: point,
        }
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn contains(&self, frame: u64) -> bool {
        frame >= self.start && frame < self.end
    }

    pub fn intersects(&self, other: SampleRange) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_end_before_start() {
        assert_eq!(
            SampleRange::new(10, 5),
            Err(RangeError::EndBeforeStart { start: 10, end: 5 })
        );
    }

    #[test]
    fn empty_range_is_allowed() {
        let r = SampleRange::new(7, 7).unwrap();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn intersection_is_half_open() {
        let a = SampleRange::new(0, 10).unwrap();
        let b = SampleRange::new(10, 20).unwrap();
        let c = SampleRange::new(5, 15).unwrap();
        assert!(!a.intersects(b), "touching ranges do not intersect");
        assert!(a.intersects(c));
        assert!(b.intersects(c));
    }

    #[test]
    fn contains_is_half_open() {
        let r = SampleRange::new(10, 20).unwrap();
        assert!(r.contains(10));
        assert!(r.contains(19));
        assert!(!r.contains(20));
        assert!(!r.contains(9));
    }
}
