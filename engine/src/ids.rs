//! Identifier newtypes.
//!
//! Distinct types for each ID class so the compiler refuses to mix them up.
//! `SourceId` is content-derived per `03-data-model.md` and lives as a
//! `String` so the canonical project-file form (e.g. `src_a4f2`) round-trips
//! without conversion. The internal IDs (track / clip / effect / profile) are
//! `u64` counters minted per project.

use std::fmt;

macro_rules! string_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub struct $name(String);

        impl $name {
            pub fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[doc = concat!("ID prefix used by the DSL surface for ", stringify!($name), ".")]
            pub const PREFIX: &'static str = $prefix;
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

macro_rules! numeric_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub struct $name(pub u64);

        impl $name {
            pub const PREFIX: &'static str = $prefix;
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{:03}", $prefix, self.0)
            }
        }
    };
}

string_id!(SourceId, "src_");
string_id!(ProfileId, "np_");
string_id!(ClipboardRef, "cb_");

numeric_id!(TrackId, "t_");
numeric_id!(ClipId, "c_");
numeric_id!(EffectInstanceId, "e_");
numeric_id!(GroupId, "g_");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_id_round_trips_through_display() {
        let id = SourceId::new("src_a4f2");
        assert_eq!(id.to_string(), "src_a4f2");
        assert_eq!(id.as_str(), "src_a4f2");
    }

    #[test]
    fn numeric_id_format_is_zero_padded() {
        assert_eq!(TrackId(7).to_string(), "t_007");
        assert_eq!(ClipId(123).to_string(), "c_123");
    }

    #[test]
    fn distinct_id_types_are_not_interchangeable() {
        // This is a compile-time guarantee; the assertion just keeps the test
        // useful at runtime as well.
        let t = TrackId(1);
        let c = ClipId(1);
        assert_eq!(format!("{t} {c}"), "t_001 c_001");
    }
}
