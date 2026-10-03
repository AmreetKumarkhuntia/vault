#![deny(clippy::print_stderr, clippy::print_stdout)]

//! Renderers for run results: terminal, JSON, JUnit XML, and offline HTML.
//! HTML adds opt-in execution evidence without changing the JSON v1 results.

mod html;
mod junit;
mod pretty;
mod redact;

pub use html::{to_html, write_html, HtmlReport};
pub use junit::to_junit_xml;
pub use pretty::print_run;
pub use redact::Redactor;

use vault_core::RunResult;

pub fn write_json(run: &RunResult, path: &str) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(run)?)
}

pub fn write_junit(run: &RunResult, path: &str) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, to_junit_xml(run))
}
