//! smeltr-core: shared types and on-disk format used by the daemon and clients.

pub mod clock;
pub mod codec;
pub mod event;

pub use clock::MonoClock;
pub use event::{Event, Payload, Source};
