use std::path::{Path, PathBuf};

use serde_yaml_ng::Value as Yaml;
use walkdir::WalkDir;

use crate::yaml::{interpolate_env, yaml_to_json};
use crate::{Config, DslError, FlowDef, TestDef};

/// Everything loaded from a suite directory.
#[derive(Debug)]
pub struct Suite {
    pub root: PathBuf,
    pub config: Config,
    pub tests: Vec<LoadedTest>,
    pub flows: Vec<LoadedFlow>,
}

#[derive(Debug)]
pub struct LoadedTest {
    pub path: PathBuf,
    pub def: TestDef,
}

#[derive(Debug)]
pub struct LoadedFlow {
    pub path: PathBuf,
    pub def: FlowDef,
}

/// Walk `root`, load `vault.yaml` + every `*.test.yaml` / `*.flow.yaml`.
/// `fixtures/` directories are never discovered as tests.
pub fn discover(root: &Path) -> Result<Suite, DslError> {
    let config_path = root.join("vault.yaml");
    let config: Config = parse_file(&config_path)?;

    let mut tests = Vec::new();
    let mut flows = Vec::new();

    for entry in WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.file_name() != "fixtures")
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".test.yaml") || name.ends_with(".test.yml") {
            let def: TestDef = parse_file(path)?;
            tests.push(LoadedTest {
                path: path.to_path_buf(),
                def,
            });
        } else if name.ends_with(".flow.yaml") || name.ends_with(".flow.yml") {
            let def: FlowDef = parse_file(path)?;
            flows.push(LoadedFlow {
                path: path.to_path_buf(),
                def,
            });
        }
    }

    Ok(Suite {
        root: root.to_path_buf(),
        config,
        tests,
        flows,
    })
}

/// Parse one YAML file into a typed value, going through the tag-aware
/// YAML→JSON conversion so `!matcher` tags survive as `$tag` objects.
pub fn parse_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, DslError> {
    let raw = std::fs::read_to_string(path).map_err(|e| DslError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    parse_str(&raw, &path.display().to_string())
}

pub fn parse_str<T: serde::de::DeserializeOwned>(raw: &str, origin: &str) -> Result<T, DslError> {
    let raw = interpolate_env(raw);
    let yaml: Yaml = serde_yaml_ng::from_str(&raw).map_err(|e| DslError::Parse {
        path: origin.into(),
        message: e.to_string(),
    })?;
    let json = yaml_to_json(&yaml);
    serde_json::from_value(json).map_err(|e| DslError::Parse {
        path: origin.into(),
        message: e.to_string(),
    })
}
