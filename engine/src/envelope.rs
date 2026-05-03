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

    /// Evaluate the breakpoint sequence at `time` (samples). Out-of-range
    /// times clamp to the first or last breakpoint's value. Returns `None`
    /// for an empty sequence.
    pub fn evaluate(&self, time: u64) -> Option<f32> {
        self.evaluate_mapped(time, |v| v)
    }

    /// Like [`Self::evaluate`] but applies `map` to each breakpoint's value
    /// before interpolation. Useful for volume envelopes, where the dB
    /// values need to be converted to linear gain so a `-inf` endpoint
    /// interpolates cleanly to silence rather than producing NaN.
    pub fn evaluate_mapped(
        &self,
        time: u64,
        map: impl Fn(f32) -> f32,
    ) -> Option<f32> {
        let pts = &self.points;
        if pts.is_empty() {
            return None;
        }
        if time <= pts[0].time {
            return Some(map(pts[0].value));
        }
        let last = &pts[pts.len() - 1];
        if time >= last.time {
            return Some(map(last.value));
        }
        let i = pts.partition_point(|p| p.time <= time) - 1;
        let a = &pts[i];
        let b = &pts[i + 1];
        let t = (time - a.time) as f32 / (b.time - a.time) as f32;
        let a_v = map(a.value);
        let b_v = map(b.value);
        Some(curve_interp(a_v, b_v, t, a.curve))
    }
}

fn curve_interp(a: f32, b: f32, t: f32, curve: CurveKind) -> f32 {
    match curve {
        CurveKind::Hold => a,
        CurveKind::Linear => a + (b - a) * t,
        CurveKind::Exponential => a + (b - a) * t * t,
        CurveKind::Logarithmic => a + (b - a) * t.sqrt(),
        CurveKind::SCurve => a + (b - a) * t * t * (3.0 - 2.0 * t),
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

    fn bp_curve(t: u64, v: f32, c: CurveKind) -> Breakpoint {
        Breakpoint {
            time: t,
            value: v,
            curve: c,
        }
    }

    #[test]
    fn evaluate_returns_none_for_empty() {
        assert_eq!(BreakpointSeq::empty().evaluate(0), None);
    }

    #[test]
    fn evaluate_clamps_outside_range() {
        let seq = BreakpointSeq::new(vec![bp(100, 0.5), bp(200, 0.8)]).unwrap();
        assert_eq!(seq.evaluate(50), Some(0.5));
        assert_eq!(seq.evaluate(500), Some(0.8));
    }

    #[test]
    fn evaluate_linear_midpoint() {
        let seq = BreakpointSeq::new(vec![
            bp_curve(0, 0.0, CurveKind::Linear),
            bp_curve(100, 1.0, CurveKind::Linear),
        ])
        .unwrap();
        assert!((seq.evaluate(50).unwrap() - 0.5).abs() < 1e-6);
        assert!((seq.evaluate(25).unwrap() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn evaluate_hold_stays_flat_until_next_breakpoint() {
        let seq = BreakpointSeq::new(vec![
            bp_curve(0, 0.0, CurveKind::Hold),
            bp_curve(100, 1.0, CurveKind::Linear),
        ])
        .unwrap();
        // Anywhere inside the [0, 100) segment is the previous value.
        assert_eq!(seq.evaluate(50), Some(0.0));
        assert_eq!(seq.evaluate(99), Some(0.0));
        assert_eq!(seq.evaluate(100), Some(1.0));
    }

    #[test]
    fn evaluate_scurve_midpoint_is_half() {
        let seq = BreakpointSeq::new(vec![
            bp_curve(0, 0.0, CurveKind::SCurve),
            bp_curve(100, 1.0, CurveKind::Linear),
        ])
        .unwrap();
        assert!((seq.evaluate(50).unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn evaluate_mapped_handles_neg_infinity_via_map() {
        // Volume envelope: dB values mapped to linear gain. -inf → 0.
        let seq = BreakpointSeq::new(vec![
            bp_curve(0, 0.0, CurveKind::Linear),
            bp_curve(100, f32::NEG_INFINITY, CurveKind::Linear),
        ])
        .unwrap();
        let to_lin = |db: f32| {
            if db == f32::NEG_INFINITY {
                0.0_f32
            } else {
                10.0_f32.powf(db / 20.0)
            }
        };
        assert!((seq.evaluate_mapped(0, to_lin).unwrap() - 1.0).abs() < 1e-6);
        assert!((seq.evaluate_mapped(50, to_lin).unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(seq.evaluate_mapped(100, to_lin), Some(0.0));
    }
}
