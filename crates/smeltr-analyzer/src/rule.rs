//! Rule trait + registry.

use crate::finding::Finding;
use smeltr_core::event::Event;

pub trait Rule: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, events: &[Event]) -> Vec<Finding>;
}

pub fn all_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(crate::rules::metal_error::MetalErrorRule),
        Box::new(crate::rules::queue_depth::QueueDepthRule),
        Box::new(crate::rules::queue_pressure::QueuePressureRule),
        Box::new(crate::rules::mlx_timing::MlxTimingRule),
        Box::new(crate::rules::system_pressure::SystemPressureRule),
        Box::new(crate::rules::duplicate_model_load::DuplicateModelLoadRule),
    ]
}
