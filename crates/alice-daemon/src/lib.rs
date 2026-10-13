//! Alice daemon library: storage service, HTTP API, MCP server and client.

pub mod client;
pub mod daemon;
pub mod discovery;
pub mod http;
pub mod mcp;
pub mod paths;
pub mod service;
pub mod spool;
pub mod startup;
pub mod worker;
pub mod writer;

pub use alice_embed::install_tls_provider as install_tls;
