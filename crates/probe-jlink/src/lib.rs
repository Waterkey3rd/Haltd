//! J-Link backend: runtime binding of the J-Link library and the single-threaded
//! actor that serializes all access to it (DESIGN §5.2).

pub mod actor;
mod ffi;
mod flash;
mod hss;
mod jlink;
mod rtt;

pub use jlink::{JLink, JLinkOptions};
