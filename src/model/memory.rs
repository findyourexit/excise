use sysinfo::System;

use super::ModelError;

pub const DEFAULT_PROCESS_MIB: usize = 512;
pub const MIN_PROCESS_MIB: usize = 128;
const MIB: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct MemoryBudget {
    process_limit: usize,
    model_limit: usize,
    headroom: usize,
    used: usize,
}

impl MemoryBudget {
    /// # Errors
    ///
    /// Returns [`ModelError::MemoryExhausted`] when `process_mib` is outside
    /// the supported process-memory range.
    pub fn from_mib(process_mib: usize) -> Result<Self, ModelError> {
        let maximum = detected_memory_limit_mib().max(MIN_PROCESS_MIB);
        if !(MIN_PROCESS_MIB..=maximum).contains(&process_mib) {
            return Err(ModelError::MemoryExhausted {
                required: process_mib.saturating_mul(MIB),
                limit: maximum.saturating_mul(MIB),
            });
        }
        let process_limit = process_mib.saturating_mul(MIB);
        let model_limit = process_limit.saturating_mul(3) / 4;
        Ok(Self {
            process_limit,
            model_limit,
            headroom: process_limit.saturating_sub(model_limit),
            used: 0,
        })
    }

    /// # Errors
    ///
    /// Returns [`ModelError::MemoryExhausted`] when no model memory is
    /// available for a staging budget.
    pub(crate) fn from_model_limit(model_limit: usize) -> Result<Self, ModelError> {
        if model_limit == 0 {
            return Err(ModelError::MemoryExhausted {
                required: 1,
                limit: 0,
            });
        }
        Ok(Self {
            process_limit: model_limit,
            model_limit,
            headroom: 0,
            used: 0,
        })
    }

    #[must_use]
    pub const fn process_limit(&self) -> usize {
        self.process_limit
    }

    #[must_use]
    pub const fn model_limit(&self) -> usize {
        self.model_limit
    }

    #[must_use]
    pub const fn headroom(&self) -> usize {
        self.headroom
    }

    #[must_use]
    pub const fn used(&self) -> usize {
        self.used
    }

    /// # Errors
    ///
    /// Returns [`ModelError::MemoryExhausted`] when reserving `bytes` would
    /// exceed the model limit.
    pub fn reserve(&mut self, bytes: usize) -> Result<(), ModelError> {
        let required = self.used.saturating_add(bytes);
        if required > self.model_limit {
            return Err(ModelError::MemoryExhausted {
                required,
                limit: self.model_limit,
            });
        }
        self.used = required;
        Ok(())
    }

    pub const fn release(&mut self, bytes: usize) {
        self.used = self.used.saturating_sub(bytes);
    }
}

#[must_use]
pub fn detected_memory_limit_mib() -> usize {
    let mut system = System::new();
    system.refresh_memory();
    let total = usize::try_from(system.total_memory()).unwrap_or(usize::MAX);
    total.saturating_mul(3) / 4 / MIB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget_uses_75_25_partition() {
        let budget = MemoryBudget::from_mib(DEFAULT_PROCESS_MIB)
            .expect("default process budget should be valid");
        assert_eq!(budget.model_limit(), 384 * MIB);
        assert_eq!(budget.headroom(), 128 * MIB);
        assert_eq!(budget.process_limit(), 512 * MIB);
    }

    #[test]
    fn model_reservation_never_crosses_hard_limit() {
        let mut budget = MemoryBudget::from_mib(MIN_PROCESS_MIB)
            .expect("minimum process budget should be valid");
        let limit = budget.model_limit();
        budget.reserve(limit).expect("limit should be reservable");
        assert!(budget.reserve(1).is_err());
        budget.release(limit);
        assert_eq!(budget.used(), 0);
    }
}
