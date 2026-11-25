//! Long-running backend and its Debug Adapter Protocol front end (DESIGN §4.2).

pub mod bridge;
pub mod control;
pub mod daemon;
pub mod dap;
pub mod diagnose;
mod http;
pub mod protocol;
mod rpc;
mod ws;

/// DAP listens one port above the HTTP/WebSocket port.
pub const DAP_PORT_OFFSET: u16 = 1;
pub const DEFAULT_PORT: u16 = 47110;
