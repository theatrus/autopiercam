//! Client-side boundaries for Chatstronomy's observatory device protocol.
//!
//! The worker uses an injected latest-frame source; it never opens a camera.
//! Credentials are stored only in the OS credential store. Sharing is opt-in.

mod media;
pub mod origin;
pub mod protocol;
pub mod service;
pub mod snapshot;
#[cfg(test)]
mod tests;
mod transport;
mod vault;
