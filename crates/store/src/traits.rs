use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::{DocMode, StoreDoc, StoreError, ValidationError, VerifyOutcome};

/// Connection config for one configured store instance.
#[derive(Debug, Clone)]
pub struct StoreConnConfig {
    /// Alias from config, e.g. "postgres" or "redis".
    pub alias: String,
    pub url: String,
    /// Driver-specific options (e.g. reset table exclusions).
    pub options: Value,
}

/// Options threaded into a single verify pass. Polling (`eventually:`) is
/// driven by the engine, which calls `verify` repeatedly; drivers stay
/// single-shot.
#[derive(Debug, Clone)]
pub struct VerifyOpts {
    /// Anchor for `!near-now`, frozen at SEED time (unix milliseconds).
    pub anchor_unix_ms: i64,
    /// Settle window for negative assertions (informational for reports).
    pub settle: Duration,
}

impl Default for VerifyOpts {
    fn default() -> Self {
        Self {
            anchor_unix_ms: 0,
            settle: Duration::from_millis(500),
        }
    }
}

/// Receipt of what a seed actually did — powers reports and error messages.
#[derive(Debug, Default)]
pub struct SeedReceipt {
    pub entries: Vec<String>,
}

/// Where a store document was declared and how it participates in a run.
///
/// Drivers can use this information to resolve file-backed entries before any
/// external service is contacted. Relative paths are based on
/// [`Self::declaring_yaml`], not on the process working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreDocContext {
    pub suite_root: PathBuf,
    pub declaring_yaml: PathBuf,
    pub scope: StoreDocScope,
}

impl StoreDocContext {
    pub fn new(
        suite_root: impl Into<PathBuf>,
        declaring_yaml: impl Into<PathBuf>,
        scope: StoreDocScope,
    ) -> Self {
        Self {
            suite_root: suite_root.into(),
            declaring_yaml: declaring_yaml.into(),
            scope,
        }
    }

    /// Compatibility context for the older suite-root-only API.
    pub fn for_suite_root(suite_root: impl Into<PathBuf>) -> Self {
        let suite_root = suite_root.into();
        Self::new(
            suite_root.clone(),
            suite_root.join("vault.yaml"),
            StoreDocScope::Global,
        )
    }

    pub fn declaring_dir(&self) -> &Path {
        self.declaring_yaml
            .parent()
            .unwrap_or(self.suite_root.as_path())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreDocScope {
    Global,
    Local,
}

impl StoreDocScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Local => "local",
        }
    }
}

/// One file selected while preparing a file-backed store document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFileSource {
    /// Index in the prepared document after glob expansion.
    pub prepared_index: usize,
    /// Index in the source YAML document before glob expansion.
    pub source_index: usize,
    pub yaml_path: String,
    pub declaring_yaml: PathBuf,
    pub scope: StoreDocScope,
    pub selector_kind: String,
    pub selector: String,
    /// User-facing path for this particular match.
    pub logical_path: PathBuf,
    /// Absolute path that will be re-resolved immediately before execution.
    pub candidate_path: PathBuf,
    /// Canonical identity frozen during preparation.
    pub resolved_path: PathBuf,
}

/// Owned, preflighted store document ready to be passed to
/// [`StateStore::seed`] without changing that long-standing interface.
#[derive(Debug, Clone)]
pub struct PreparedStoreDoc {
    doc: StoreDoc,
    files: Vec<ResolvedFileSource>,
}

impl PreparedStoreDoc {
    pub fn new(doc: StoreDoc, files: Vec<ResolvedFileSource>) -> Self {
        Self { doc, files }
    }

    pub fn passthrough(doc: StoreDoc) -> Self {
        Self::new(doc, Vec::new())
    }

    pub fn doc(&self) -> &StoreDoc {
        &self.doc
    }

    pub fn files(&self) -> &[ResolvedFileSource] {
        &self.files
    }

    pub fn into_doc(self) -> StoreDoc {
        self.doc
    }

    /// Append another prepared list document in declaration order.
    ///
    /// Canonical file identities are checked here so independently prepared
    /// global and test-local documents cannot execute the same file twice.
    pub fn append(&mut self, mut other: Self) -> Result<(), ValidationError> {
        let offset = self
            .doc
            .as_array()
            .map(Vec::len)
            .ok_or_else(|| ValidationError::new("seed", "prepared document must be a list"))?;
        let other_entries = other
            .doc
            .as_array_mut()
            .ok_or_else(|| ValidationError::new("seed", "prepared document must be a list"))?;

        let mut seen: HashMap<PathBuf, ResolvedFileSource> = self
            .files
            .iter()
            .map(|source| (source.resolved_path.clone(), source.clone()))
            .collect();
        for source in &other.files {
            if let Some(previous) = seen.get(&source.resolved_path) {
                return Err(ValidationError::new(
                    &source.yaml_path,
                    format!(
                        "SQL fixture `{}` selected by {} `{}` in `{}` resolves to `{}`, already selected by {} `{}` at {} in `{}`",
                        source.logical_path.display(),
                        source.selector_kind,
                        source.selector,
                        source.declaring_yaml.display(),
                        source.resolved_path.display(),
                        previous.selector_kind,
                        previous.selector,
                        previous.yaml_path,
                        previous.declaring_yaml.display()
                    ),
                ));
            }
            seen.insert(source.resolved_path.clone(), source.clone());
        }

        self.doc
            .as_array_mut()
            .expect("prepared list checked above")
            .append(other_entries);
        for source in &mut other.files {
            source.prepared_index += offset;
        }
        self.files.append(&mut other.files);
        Ok(())
    }
}

/// Opaque snapshot for watch mode; the driver that took it diffs it.
pub type Snapshot = Value;

/// Simple tabular result for step-mode inspection.
#[derive(Debug, Default, serde::Serialize)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// One per store KIND ("postgres", "redis", later "mysql"), registered by the CLI.
#[async_trait]
pub trait StoreDriver: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Static validation at suite-load time. Templates appear as placeholder
    /// strings; no I/O happens here.
    fn validate(&self, doc: &StoreDoc, mode: DocMode) -> Result<(), ValidationError>;

    /// Suite-aware validation for drivers with file-backed documents.
    ///
    /// The provided implementation preserves compatibility for existing
    /// drivers by delegating to [`StoreDriver::validate`]. Drivers that need a
    /// filesystem boundary can override this method while legacy callers can
    /// continue using `validate`.
    fn validate_with_suite_root(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        _suite_root: &Path,
    ) -> Result<(), ValidationError> {
        self.validate(doc, mode)
    }

    /// Validation with the exact YAML declaration site available to drivers.
    /// Existing drivers remain source-compatible and retain their suite-root
    /// behavior through the default implementation.
    fn validate_with_context(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        context: &StoreDocContext,
    ) -> Result<(), ValidationError> {
        self.validate_with_suite_root(doc, mode, &context.suite_root)
    }

    /// Resolve file-backed entries into an owned execution document.
    ///
    /// Drivers without file-backed documents simply validate and clone.
    fn prepare_with_context(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        context: &StoreDocContext,
    ) -> Result<PreparedStoreDoc, ValidationError> {
        self.validate_with_context(doc, mode, context)?;
        Ok(PreparedStoreDoc::passthrough(doc.clone()))
    }

    async fn connect(&self, cfg: &StoreConnConfig) -> Result<Arc<dyn StateStore>, StoreError>;
}

/// One per configured INSTANCE. Flattened facade: every driver implements
/// all roles (seed / verify / reset / watch / inspect).
#[async_trait]
pub trait StateStore: Send + Sync {
    fn kind(&self) -> &'static str;
    fn alias(&self) -> &str;

    async fn ping(&self) -> Result<(), StoreError>;

    async fn reset(&self, spec: &StoreDoc) -> Result<(), StoreError>;

    async fn seed(&self, doc: &StoreDoc) -> Result<SeedReceipt, StoreError>;

    /// Watch mode: capture per-table state before steps run.
    async fn snapshot(&self, doc: &StoreDoc) -> Result<Snapshot, StoreError>;

    /// Watch mode: recompute and diff against a prior snapshot.
    async fn diff_snapshot(
        &self,
        doc: &StoreDoc,
        before: &Snapshot,
    ) -> Result<VerifyOutcome, StoreError>;

    /// NEVER `Err` for assertion failures — those are data in `VerifyOutcome`.
    /// `Err` is reserved for harness faults (connection lost, type error).
    async fn verify(&self, doc: &StoreDoc, opts: &VerifyOpts) -> Result<VerifyOutcome, StoreError>;

    /// Step-mode, read-only inspection ("db <sql>" / "redis <cmd>").
    async fn inspect(&self, query: &str) -> Result<Table, StoreError>;
}
