//! apisip: point it at an OpenAPI spec, get JSONL or Parquet files.
//!
//! The binary is the interface. The library exposes only what the binary and
//! the integration tests use. It is not an embedding API.

pub(crate) mod auth;
pub mod cmd;
pub(crate) mod config;
pub(crate) mod detect;
pub mod envfile;
pub(crate) mod http;
pub(crate) mod incremental;
pub(crate) mod prompt;
pub(crate) mod schema;
pub(crate) mod sink;
pub(crate) mod spec;
pub mod state;
pub(crate) mod views;
pub mod workspace;
