use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use vault_store::{
    DocMode, StateStore, StoreConnConfig, StoreDoc, StoreDocContext, StoreDocScope, StoreDriver,
    StoreError, ValidationError,
};

struct LegacyDriver;

#[async_trait]
impl StoreDriver for LegacyDriver {
    fn kind(&self) -> &'static str {
        "legacy"
    }

    fn validate(&self, _doc: &StoreDoc, _mode: DocMode) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn connect(&self, _cfg: &StoreConnConfig) -> Result<Arc<dyn StateStore>, StoreError> {
        Err(StoreError::Connection("not used by this test".into()))
    }
}

#[test]
fn existing_drivers_and_connection_config_literals_remain_source_compatible() {
    let config = StoreConnConfig {
        alias: "legacy-instance".into(),
        url: "legacy://example".into(),
        options: json!({}),
    };
    assert_eq!(config.alias, "legacy-instance");

    LegacyDriver
        .validate_with_suite_root(&json!([]), DocMode::Seed, Path::new("."))
        .expect("the suite-root hook should delegate to legacy validate implementations");

    let prepared = LegacyDriver
        .prepare_with_context(
            &json!([]),
            DocMode::Seed,
            &StoreDocContext::new(".", "./nested/test.yaml", StoreDocScope::Local),
        )
        .expect("the preparation hook should validate and clone for legacy drivers");
    assert_eq!(prepared.doc(), &json!([]));
    assert!(prepared.files().is_empty());
}
