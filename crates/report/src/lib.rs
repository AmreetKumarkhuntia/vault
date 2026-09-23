//! Renderers for run results: pretty terminal, JSON, JUnit XML.
//! All three consume the same structs — the JSON report is lossless with
//! respect to the terminal output, never a re-parse of it.

mod junit;
mod pretty;

pub use junit::to_junit_xml;
pub use pretty::print_run;

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
