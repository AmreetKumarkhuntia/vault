//! `vault-store` — the leaf crate of the workspace.
//!
//! Holds the [`StoreDriver`]/[`StateStore`] trait boundary every datastore
//! driver implements, the report-shaped outcome types ([`VerifyOutcome`],
//! [`CheckFailure`], [`NearMiss`], [`FieldDiff`]), and the shared matcher
//! vocabulary (`matchers`) used by DB verification, Redis verification and
//! outbound-call verification alike.

pub mod matchers;
mod outcome;
mod registry;
mod traits;

pub use outcome::*;
pub use registry::StoreRegistry;
pub use traits::*;

use serde_json::Value;

/// A rendered store block from the YAML DSL, converted to JSON.
/// The schema of the document is owned by the driver, not the engine.
pub type StoreDoc = Value;

/// What a document is used for; drivers validate per-mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocMode {
    Seed,
    Verify,
    Reset,
    Watch,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("{0}")]
    Harness(String),
}

#[derive(Debug, thiserror::Error)]
#[error("{yaml_path}: {message}")]
pub struct ValidationError {
    pub yaml_path: String,
    pub message: String,
}

impl ValidationError {
    pub fn new(yaml_path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            yaml_path: yaml_path.into(),
            message: message.into(),
        }
    }
}
