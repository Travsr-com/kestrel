//! Money as integer millionths of a dollar.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Money as integer millionths of a dollar. Never f64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MicroUsd(pub i64);

impl MicroUsd {
    pub const MAX: MicroUsd = MicroUsd(i64::MAX);

    pub fn checked_add(self, other: i64) -> Option<MicroUsd> {
        self.0.checked_add(other).map(MicroUsd)
    }
    pub fn checked_sub(self, other: i64) -> Option<MicroUsd> {
        self.0.checked_sub(other).map(MicroUsd)
    }
    /// Never goes below zero.
    pub fn saturating_sub(self, other: MicroUsd) -> MicroUsd {
        MicroUsd(self.0.saturating_sub(other.0).max(0))
    }
}

impl fmt::Display for MicroUsd {
    /// `$1.234567`; negative amounts print as `-$1.234567`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        write!(f, "{sign}${}.{:06}", abs / 1_000_000, abs % 1_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_display() {
        assert_eq!(MicroUsd(-1_500_000).to_string(), "-$1.500000");
        assert_eq!(MicroUsd(i64::MIN).to_string(), "-$9223372036854.775808");
    }
}
