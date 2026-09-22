use getrandom::Error as RandomError;

/// Opaque identity for one private, canonical scan session.
///
/// It distinguishes independently created sessions even when their generation
/// counters have the same value.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScanSessionId([u8; 16]);

impl ScanSessionId {
    /// Returns a fresh private scan session identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot provide session entropy.
    pub fn random() -> Result<Self, RandomError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Lifecycle of one deterministic canonical scan generation.
///
/// Terminal states describe the whole generation. A partial scanner-input
/// prefix is never represented as complete: capacity exhaustion before
/// canonical reduction is `Incomplete`. `SummaryOnly` is reserved for a
/// capacity failure after a complete deterministic directory reduction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ScanGenerationState {
    Creating = 1,
    Scanning = 2,
    Reducing = 3,
    Published = 4,
    Incomplete = 5,
    Cancelled = 6,
    SummaryOnly = 7,
}

impl ScanGenerationState {
    #[must_use]
    pub const fn from_code(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Creating),
            2 => Some(Self::Scanning),
            3 => Some(Self::Reducing),
            4 => Some(Self::Published),
            5 => Some(Self::Incomplete),
            6 => Some(Self::Cancelled),
            7 => Some(Self::SummaryOnly),
            _ => None,
        }
    }

    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Creating,
                Self::Scanning | Self::Incomplete | Self::Cancelled
            ) | (
                Self::Scanning,
                Self::Reducing | Self::SummaryOnly | Self::Incomplete | Self::Cancelled
            ) | (
                Self::Reducing,
                Self::Published | Self::SummaryOnly | Self::Incomplete | Self::Cancelled
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_lifecycle_exposes_summary_only_as_a_terminal_state() {
        assert!(ScanGenerationState::Creating.can_transition_to(ScanGenerationState::Scanning));
        assert!(ScanGenerationState::Scanning.can_transition_to(ScanGenerationState::Reducing));
        assert!(ScanGenerationState::Reducing.can_transition_to(ScanGenerationState::Published));
        assert!(ScanGenerationState::Scanning.can_transition_to(ScanGenerationState::SummaryOnly));
        assert!(ScanGenerationState::Reducing.can_transition_to(ScanGenerationState::SummaryOnly));
        assert!(ScanGenerationState::Scanning.can_transition_to(ScanGenerationState::Incomplete));
        assert!(!ScanGenerationState::Published.can_transition_to(ScanGenerationState::Scanning));
        assert_eq!(ScanGenerationState::from_code(8), None);
    }
}
