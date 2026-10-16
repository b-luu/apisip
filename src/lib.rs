//! apisip: point it at an OpenAPI spec, get JSONL or Parquet files.
//!
//! The binary is the main interface; the library exposes the same pieces
//! for tests and for embedding.

pub mod cmd;
pub mod config;
pub mod detect;
pub mod envfile;
pub mod prompt;
pub mod spec;
pub mod state;
pub mod workspace;
