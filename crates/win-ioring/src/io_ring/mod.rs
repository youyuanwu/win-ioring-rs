//! Raw, unsafe bindings-level access to the Windows IoRing API.
//!
//! This module wraps every IoRing entry point the platform bindings expose. It
//! performs no lifetime tracking: buffers and handles referenced by a submitted
//! operation must be kept alive by the caller until that operation's completion
//! is dequeued. For a safe API that tracks these lifetimes, see
//! [`crate::runtime`].

mod api;
pub use api::{BufferInfo, Capabilities, IoRing, IoRingBuilder, RingInfo};
pub mod ops;
#[cfg(test)]
mod tests;
