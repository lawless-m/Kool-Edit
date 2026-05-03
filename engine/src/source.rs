//! Source: a logical audio file. Owns its destructive edit history.
//!
//! The on-disk samples (in OPFS for the browser, on the filesystem in tests)
//! are referenced by `base_file`. The actual reads happen via the storage
//! layer; this struct only owns the descriptor and the operation journal.

use crate::edit_list::EditList;
use crate::ids::SourceId;
use crate::op::Op;

/// Path to a chunk of audio in the storage layer (OPFS or native filesystem).
/// Stored as a string so it round-trips through the JSON project file
/// unchanged (per `03-data-model.md` §Persistence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoragePath(pub String);

impl StoragePath {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Wall-clock timestamp (RFC 3339 string for now). Will likely become a typed
/// instant once the persistence layer lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timestamp(pub String);

#[derive(Clone, Debug)]
pub struct Source {
    pub id: SourceId,
    pub name: String,
    pub channel_count: u16,
    pub sample_rate: u32,
    pub base_file: StoragePath,
    pub base_length: u64,
    pub edits: EditList<Op>,
    pub peak_cache: Option<StoragePath>,
    pub created_at: Timestamp,
    pub modified_at: Timestamp,
}

impl Source {
    pub fn new(
        id: SourceId,
        name: impl Into<String>,
        channel_count: u16,
        sample_rate: u32,
        base_file: StoragePath,
        base_length: u64,
        created_at: Timestamp,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            channel_count,
            sample_rate,
            base_file,
            base_length,
            edits: EditList::new(),
            peak_cache: None,
            modified_at: created_at.clone(),
            created_at,
        }
    }

    /// Append a new destructive op, advancing the history pointer and bumping
    /// the modification timestamp. Truncates any redo branch.
    pub fn apply(&mut self, op: Op, modified_at: Timestamp) {
        self.edits.apply(op);
        self.modified_at = modified_at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::SampleRange;

    fn fixture() -> Source {
        Source::new(
            SourceId::new("src_a4f2"),
            "vocals_raw.wav",
            1,
            48_000,
            StoragePath::new("sources/src_a4f2/base.f32"),
            14_400_000,
            Timestamp("2026-05-03T12:00:00Z".into()),
        )
    }

    #[test]
    fn newly_created_source_has_no_edits() {
        let s = fixture();
        assert!(s.edits.is_empty());
        assert_eq!(s.edits.pointer(), 0);
        assert_eq!(s.created_at, s.modified_at);
    }

    #[test]
    fn applying_an_op_advances_the_pointer_and_updates_modified_at() {
        let mut s = fixture();
        let op = Op::Silence {
            range: SampleRange::new(100, 200).unwrap(),
        };
        s.apply(op, Timestamp("2026-05-03T14:23:11Z".into()));
        assert_eq!(s.edits.len(), 1);
        assert_eq!(s.edits.pointer(), 1);
        assert_ne!(s.created_at, s.modified_at);
    }
}
