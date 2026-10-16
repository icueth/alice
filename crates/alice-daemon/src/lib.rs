//! Alice daemon library: storage service, HTTP API, MCP server and client.

pub mod backup;
pub mod client;
pub mod config;
pub mod daemon;
pub mod discovery;
pub mod doctor;
pub mod embedding;
pub mod http;
pub mod judge_http;
pub mod judge_worker;
pub mod maintenance;
pub mod mcp;
pub mod paths;
pub mod precise_gate;
pub mod redaction;
pub mod reembed;
pub mod restore;
pub mod secrets;
pub mod service;
pub mod service_install;
pub mod spool;
pub mod startup;
pub mod wal;
pub mod worker;
pub mod writer;

pub use alice_embed::install_tls_provider as install_tls;
