//! rustranslator — translation engine.
//!
//! Module and function names deliberately mirror the Python pipeline this project replaces
//! (`pipeline_ptbr.py`), so the migration can be checked function by function and anyone who
//! knew the Python recognises the Rust.

pub mod config;
pub mod llm;
pub mod progress;
pub mod output;
