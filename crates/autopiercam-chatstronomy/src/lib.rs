//! Client-side boundaries for Chatstronomy's observatory device protocol.
//!
//! This crate performs no network I/O, opens no cameras, and stores no secrets.
//! The future connection worker must enforce these checks again before sending,
//! and cancel pending work on consent or capture-session changes.

pub mod origin;
pub mod protocol;
pub mod snapshot;
