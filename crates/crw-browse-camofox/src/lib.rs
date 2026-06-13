//! MCP server bridging MCP clients to the camofox-browser REST API for
//! interactive, stateful browser automation on Camoufox (Firefox).
//!
//! This is an additive sibling to `crw-browse` (which drives CDP). It does not
//! depend on or modify `crw-browse`; pick whichever backend your browser is.

pub mod camofox;
pub mod server;
