//! rustranslator — the translation engine.
//!
//! The shape of the domain, end to end: a document becomes **blocks**, a block becomes a
//! **translation**, a translation becomes a **verdict**, and only the flagged verdicts are ever
//! read by a person. Each module is one of those steps.
//!
//!   `parser`    document -> blocks
//!   `llm`       a block -> a translation, and (source, translation) -> a verdict
//!   `output`    where the translated document lands
//!   `progress`  the registry on disk, and what it says about a job
//!   `config`    which server, which model, which locale

pub mod config;
pub mod llm;
pub mod output;
pub mod parser;
pub mod progress;
