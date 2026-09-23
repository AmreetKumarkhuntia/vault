//! `vault-core` — the execution engine.

mod assert_http;
mod capture;
mod engine;
mod http;
mod result;
mod template;

pub use assert_http::assert_response;
pub use capture::capture_value;
pub use engine::{FlowOutcome, Gate, GateDecision, NoopGate, PausePoint, PauseSnapshot, TestRunner};
pub use http::{execute_request, StepResponse};
pub use result::*;
pub use template::TemplateEngine;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("template error: {0}")]
    Template(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("store error: {0}")]
    Store(#[from] vault_store::StoreError),
    #[error("{0}")]
    Harness(String),
}
