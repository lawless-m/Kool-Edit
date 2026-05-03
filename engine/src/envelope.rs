//! Breakpoint envelopes for clip and track automation.
//!
//! Doc 03 §"Constraints and invariants" #4: breakpoints are sorted by time
//! and have unique times. Both invariants are enforced at construction; once
//! built, the only way to mutate is through methods that preserve them.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CurveKind {
    Linear,
    Exponential,
    Logarithmic,
    Hold,
    SCurve,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Breakpoint {
    pub time: u64,
    pub value: f32,
    pub curve: CurveKind,
}

#[derive(Debug, PartialEq)]
pub enum BreakpointError {
    UnsortedOrDuplicateTime { previous: u64, next: u64 },
}

impl fmt::Display for BreakpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BreakpointError::UnsortedOrDuplicateTime { previous, next } => write!(
                f,
                "breakpoint times must be strictly increasing (got {previous} then {next})"
            ),
        }
    }
}

impl std::error::Error for BreakpointError {}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(try_from = "Vec<Breakpoint>", into = "Vec<Breakpoint>")]
pub struct BreakpointSeq {
    points: Vec<Breakpoint>,
}

impl TryFrom<Vec<Breakpoint>> for BreakpointSeq {
    type Error = BreakpointError;
    fn try_from(points: Vec<Breakpoint>) -> Result<Self, Self::Error> {
        Self::new(points)
    }
}

impl From<BreakpointSeq> for Vec<Breakpoint> {
    fn from(seq: BreakpointSeq) -> Self {
        seq.points
    }
}

impl BreakpointSeq {
    pub fn new(points: Vec<Breakpoint>) -> Result<Self, BreakpointError> {
        for pair in points.windows(2) {
            if pair[1].time <= pair[0].time {
                return Err(BreakpointError::UnsortedOrDuplicateTime {
                    previous: pair[0].time,
                    next: pair[1].time,
                });
            }
        }
        Ok(Self { points })
    }

    pub fn empty() -> Self {
        Self { points: Vec::new() }
    }

    pub fn points(&self) -> &[Breakpoint] {
        &self.points
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Insert `bp`, preserving ordering. Returns an error if its time would
    /// duplicate an existing breakpoint.
    pub fn insert(&mut self, bp: Breakpoint) -> Result<(), BreakpointError> {
        let pos = self.points.partition_point(|p| p.time < bp.time);
        if let Some(existing) = self.points.get(pos) {
            if existing.time == bp.time {
                return Err(BreakpointError::UnsortedOrDuplicateTime {
                    previous: existing.time,
                    next: bp.time,
                });
            }
        }
        self.points.insert(pos, bp);
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EnvelopeParam {
    Volume,
    Pan,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipEnvelope {
    pub parameter: EnvelopeParam,
    pub breakpoints: BreakpointSeq,
}

/// Dotted path identifying which parameter an automation lane drives, e.g.
/// `"track.gain"`, `"insert.1.threshold"`. Free-form for now; the DSL parser
/// will validate against the actual parameter graph when it lands.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParamPath(pub String);

impl ParamPath {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationLane {
    pub parameter: ParamPath,
    pub breakpoints: BreakpointSeq,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bp(t: u64, v: f32) -> Breakpoint {
        Breakpoint {
            time: t,
            value: v,
            curve: CurveKind::Linear,
        }
    }

    #[test]
    fn rejects_unsorted_breakpoints() {
        let err =
            BreakpointSeq::new(vec![bp(10, 0.0), bp(5, 0.0)]).unwrap_err();
        assert!(matches!(
            err,
            BreakpointError::UnsortedOrDuplicateTime { previous: 10, next: 5 }
        ));
    }

    #[test]
    fn rejects_duplicate_times() {
        let err =
            BreakpointSeq::new(vec![bp(10, 0.0), bp(10, 1.0)]).unwrap_err();
        assert!(matches!(
            err,
            BreakpointError::UnsortedOrDuplicateTime { previous: 10, next: 10 }
        ));
    }

    #[test]
    fn insert_keeps_ordering() {
        let mut seq = BreakpointSeq::new(vec![bp(10, 0.0), bp(30, 0.0)]).unwrap();
        seq.insert(bp(20, 0.5)).unwrap();
        let times: Vec<u64> = seq.points().iter().map(|p| p.time).collect();
        assert_eq!(times, vec![10, 20, 30]);
    }

    #[test]
    fn insert_rejects_duplicate_time() {
        let mut seq = BreakpointSeq::new(vec![bp(10, 0.0)]).unwrap();
        let err = seq.insert(bp(10, 1.0)).unwrap_err();
        assert!(matches!(
            err,
            BreakpointError::UnsortedOrDuplicateTime { .. }
        ));
    }
}
