//! `vault-dsl` — the declarative YAML test language.
//!
//! Parses `vault.yaml` (global config), `*.test.yaml` (tests) and
//! `*.flow.yaml` (cross-test flows) into typed structures. Store blocks
//! (`seed.postgres:`, `verify.redis:` …) are deliberately kept as raw JSON
//! values — their schema is owned by the drivers (`vault-store-*`), which
//! is what makes new stores pure additions.
//!
//! YAML tag matchers (`!any`, `!uuid`, `!near-now 5s`, …) are converted to
//! `{"$tag": "...", "arg": ...}` JSON objects understood by
//! `vault_store::matchers`.

mod discover;
mod schema;
mod validate;
mod yaml;

pub use discover::{discover, parse_file, parse_str, LoadedFlow, LoadedTest, Suite};
pub use schema::*;
pub use validate::validate_suite;
pub use yaml::{interpolate_env, yaml_to_json};

#[derive(Debug, thiserror::Error)]
pub enum DslError {
    #[error("{path}: {message}")]
    Parse { path: String, message: String },
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}
