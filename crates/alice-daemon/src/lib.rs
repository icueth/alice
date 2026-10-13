//! Alice daemon: the single writer, the embedding worker and the service layer that the HTTP
//! API, the MCP server and the CLI all call.

pub mod paths;
pub mod service;
pub mod worker;
pub mod writer;
