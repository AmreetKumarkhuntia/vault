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
        Self { anchor_unix_ms: 0, settle: Duration::from_millis(500) }
    }
}

/// Receipt of what a seed actually did — powers reports and error messages.
#[derive(Debug, Default)]
pub struct SeedReceipt {
    pub entries: Vec<String>,
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
