//! Rule trait + registry.

use crate::finding::Finding;
use smeltr_core::event::Event;

pub trait Rule: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, events: &[Event]) -> Vec<Finding>;
}

pub fn all_rules() -> Vec<Box<dyn Rule>> {
    // Populated in Task 6.
    Vec::new()
}
