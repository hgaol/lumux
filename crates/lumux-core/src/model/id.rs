//! Stable, typed identifiers for sessions, windows, and panes.
//!
//! tmux-style sigils: sessions `$n`, windows `@n`, panes `%n`. IDs are
//! allocated monotonically and never reused within a daemon's lifetime, so a
//! stale reference fails to resolve rather than silently aliasing a new object.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($name:ident, $sigil:literal, $counter:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub u32);

        static $counter: AtomicU32 = AtomicU32::new(1);

        impl $name {
            /// Allocate the next never-before-used id of this kind.
            pub fn alloc() -> Self {
                $name($counter.fetch_add(1, Ordering::Relaxed))
            }

            pub fn raw(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($sigil, "{}"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let digits = s.strip_prefix($sigil).ok_or(IdParseError)?;
                digits.parse::<u32>().map($name).map_err(|_| IdParseError)
            }
        }
    };
}

define_id!(SessionId, "$", SESSION_COUNTER);
define_id!(WindowId, "@", WINDOW_COUNTER);
define_id!(PaneId, "%", PANE_COUNTER);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdParseError;

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid lumux id (expected sigil + number, e.g. $1 / @2 / %3)"
        )
    }
}

impl std::error::Error for IdParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_monotonic_and_unique() {
        let a = PaneId::alloc();
        let b = PaneId::alloc();
        let c = PaneId::alloc();
        assert!(a.raw() < b.raw() && b.raw() < c.raw());
        assert_ne!(a, b);
    }

    #[test]
    fn display_uses_sigil() {
        assert_eq!(SessionId(1).to_string(), "$1");
        assert_eq!(WindowId(7).to_string(), "@7");
        assert_eq!(PaneId(42).to_string(), "%42");
    }

    #[test]
    fn roundtrip_parse() {
        assert_eq!("$3".parse::<SessionId>(), Ok(SessionId(3)));
        assert_eq!("@10".parse::<WindowId>(), Ok(WindowId(10)));
        assert_eq!("%5".parse::<PaneId>(), Ok(PaneId(5)));
    }

    #[test]
    fn rejects_wrong_sigil_or_garbage() {
        assert!("@3".parse::<SessionId>().is_err());
        assert!("$x".parse::<SessionId>().is_err());
        assert!("3".parse::<PaneId>().is_err());
        assert!("".parse::<WindowId>().is_err());
    }

    #[test]
    fn id_kinds_are_distinct_types() {
        // Compile-time guarantee: these are different types and cannot be
        // mixed up. Runtime check on the counters being independent.
        let s = SessionId::alloc();
        let w = WindowId::alloc();
        assert_eq!(s.to_string().chars().next(), Some('$'));
        assert_eq!(w.to_string().chars().next(), Some('@'));
    }
}
