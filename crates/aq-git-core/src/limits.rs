use crate::{Error, Result};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
/// Shared execution budget: coalesced callers do not consume additional slots.
pub struct HeavyBudget {
    limit: usize,
    active: AtomicUsize,
}
pub struct HeavyPermit {
    budget: Arc<HeavyBudget>,
}
impl HeavyBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            active: AtomicUsize::new(0),
        })
    }
    pub fn acquire(self: &Arc<Self>) -> Result<HeavyPermit> {
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= self.limit {
                return Err(Error::new(
                    "overloaded",
                    "heavy operation execution budget exhausted",
                ));
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(HeavyPermit {
                        budget: self.clone(),
                    });
                }
                Err(current) => active = current,
            }
        }
    }
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
}
impl Drop for HeavyPermit {
    fn drop(&mut self) {
        self.budget.active.fetch_sub(1, Ordering::Release);
    }
}
