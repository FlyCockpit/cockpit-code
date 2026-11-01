//! ACP front end (milestone 9): `cockpit-code acp` serves the Agent Client
//! Protocol v1 over stdio for editors such as Zed.
//!
//! Adapted from FlyCockpit's Cockpit: the LF codec (`codec`), the
//! bounded outbound request registry (`rpc`) and the permission-option
//! mapping (`mapping`). Everything else is new: there is no daemon, so each
//! ACP session owns an in-process `code_engine::Session`.
//!
//! Protocol: `agent-client-protocol-schema =1.7.0` (Cockpit's pin), stable v1
//! surface only. Payloads are built as JSON and, in debug builds, checked
//! against the schema types on the way out.

mod codec;
mod mapping;
mod replay;
mod rpc;
mod server;
mod session;

pub use server::{Options, run};
