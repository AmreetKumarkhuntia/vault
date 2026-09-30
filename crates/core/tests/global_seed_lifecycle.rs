use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use vault_core::{NoopGate, TestRunner, TestStatus};
use vault_dsl::{parse_str, Defaults, FlowDef, TestDef};
use vault_mock::MockServer;
use vault_store::{
    SeedReceipt, Snapshot, StateStore, StoreDoc, StoreError, Table, VerifyOpts, VerifyOutcome,
};

#[derive(Default)]
struct StoreState {
    events: Mutex<Vec<String>>,
    seed_docs: Mutex<Vec<Value>>,
    fail_seed_at: Mutex<Option<usize>>,
}

struct RecordingStore {
    state: Arc<StoreState>,
}

#[async_trait]
impl StateStore for RecordingStore {
    fn kind(&self) -> &'static str {
        "postgres"
    }

    fn alias(&self) -> &str {
        "postgres"
    }

    async fn ping(&self) -> Result<(), StoreError> {
        Ok(())
    }

    async fn reset(&self, _spec: &StoreDoc) -> Result<(), StoreError> {
        self.state.events.lock().unwrap().push("reset".into());
        Ok(())
    }

    async fn seed(&self, doc: &StoreDoc) -> Result<SeedReceipt, StoreError> {
        let call = {
            let mut docs = self.state.seed_docs.lock().unwrap();
            docs.push(doc.clone());
            docs.len()
        };
        self.state.events.lock().unwrap().push("seed".into());
        if *self.state.fail_seed_at.lock().unwrap() == Some(call) {
            return Err(StoreError::Harness(format!("seed call {call} failed")));
        }
        Ok(SeedReceipt {
            entries: vec![format!("seed-{call}")],
        })
    }

    async fn snapshot(&self, _doc: &StoreDoc) -> Result<Snapshot, StoreError> {
        Ok(Value::Null)
    }

    async fn diff_snapshot(
        &self,
        _doc: &StoreDoc,
        _before: &Snapshot,
    ) -> Result<VerifyOutcome, StoreError> {
        Ok(VerifyOutcome::default())
    }

    async fn verify(
        &self,
        _doc: &StoreDoc,
        _opts: &VerifyOpts,
    ) -> Result<VerifyOutcome, StoreError> {
        Ok(VerifyOutcome::default())
    }

    async fn inspect(&self, _query: &str) -> Result<Table, StoreError> {
        Ok(Table::default())
    }
}

async fn runner(global_seed: Value, fail_seed_at: Option<usize>) -> (TestRunner, Arc<StoreState>) {
    let state = Arc::new(StoreState {
        fail_seed_at: Mutex::new(fail_seed_at),
        ..StoreState::default()
    });
    let mut stores: IndexMap<String, Arc<dyn StateStore>> = IndexMap::new();
    stores.insert(
        "postgres".into(),
        Arc::new(RecordingStore {
            state: state.clone(),
        }),
    );
    let mut suite_seed = IndexMap::new();
    suite_seed.insert("postgres".into(), global_seed);

    (
        TestRunner {
            client: reqwest::Client::new(),
            target_base_url: "http://example.test".into(),
            defaults: Defaults::default(),
            global_seed: suite_seed,
            stores,
            mock: Arc::new(MockServer::start("127.0.0.1:0").await.unwrap()),
            gate: Arc::new(NoopGate),
            abort_run: std::sync::atomic::AtomicBool::new(false),
        },
        state,
    )
}

fn test_def(raw: &str) -> TestDef {
    parse_str(raw, "test.test.yaml").unwrap()
}

fn flow_def(raw: &str) -> FlowDef {
    parse_str(raw, "flow.flow.yaml").unwrap()
}

#[tokio::test]
async fn reset_boundary_seeds_global_then_local_in_one_store_call() {
    let (runner, state) = runner(json!([{"sql": "global"}]), None).await;
    let test = test_def(
        r#"
test: local
vars: { suffix: rendered }
seed:
  postgres:
    - sql: "local {{ vars.suffix }}"
"#,
    );

    let result = runner
        .run_test(&test, &IndexMap::new(), true, None, None)
        .await;

    assert_eq!(result.status, TestStatus::Passed);
    assert_eq!(&*state.events.lock().unwrap(), &["reset", "seed"]);
    assert_eq!(
        &*state.seed_docs.lock().unwrap(),
        &[json!([{"sql": "global"}, {"sql": "local rendered"}])]
    );
}

#[tokio::test]
async fn no_reset_stage_uses_only_its_local_seed() {
    let (runner, state) = runner(json!([{"sql": "global"}]), None).await;
    let test = test_def(
        r#"
test: local
seed:
  postgres:
    - sql: local
"#,
    );

    let result = runner
        .run_test(&test, &IndexMap::new(), false, None, None)
        .await;

    assert_eq!(result.status, TestStatus::Passed);
    assert_eq!(&*state.events.lock().unwrap(), &["seed"]);
    assert_eq!(
        &*state.seed_docs.lock().unwrap(),
        &[json!([{"sql": "local"}])]
    );
}

#[tokio::test]
async fn incompatible_global_and_local_documents_fail_before_reset() {
    let (runner, state) = runner(json!({"sql": "global"}), None).await;
    let test = test_def(
        r#"
test: local
seed:
  postgres:
    - sql: local
"#,
    );

    let result = runner
        .run_test(&test, &IndexMap::new(), true, None, None)
        .await;

    assert_eq!(result.status, TestStatus::Errored);
    assert!(result
        .error
        .as_deref()
        .unwrap()
        .contains("must both be arrays"));
    assert!(state.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_once_waits_past_skipped_and_pre_lifecycle_failed_stages() {
    let (runner, state) = runner(json!([{"sql": "global"}]), None).await;
    let skipped = test_def("test: skipped\nskip: not-ready\n");
    let bad_with = test_def("test: bad-with\n");
    let runnable = test_def(
        r#"
test: runnable
seed:
  postgres:
    - sql: local
"#,
    );
    let flow = flow_def(
        r#"
flow: pending reset
reset: once
on_failure: continue
stages:
  - test: skipped
  - test: bad-with
    with: { broken: "{{ ??? }}" }
  - test: runnable
"#,
    );
    let tests = HashMap::from([
        (skipped.test.clone(), &skipped),
        (bad_with.test.clone(), &bad_with),
        (runnable.test.clone(), &runnable),
    ]);

    let outcome = runner.run_flow(&flow, &tests).await;

    assert_eq!(outcome.results[0].status, TestStatus::Skipped);
    assert_eq!(outcome.results[1].status, TestStatus::Errored);
    assert_eq!(outcome.results[2].status, TestStatus::Passed);
    assert_eq!(&*state.events.lock().unwrap(), &["reset", "seed"]);
    assert_eq!(
        &*state.seed_docs.lock().unwrap(),
        &[json!([{"sql": "global"}, {"sql": "local"}])]
    );
}

#[tokio::test]
async fn reset_once_seeds_global_once_then_only_stage_local_data() {
    let (runner, state) = runner(json!([{"sql": "global"}]), None).await;
    let first = test_def(
        r#"
test: first
seed:
  postgres:
    - sql: first-local
"#,
    );
    let no_seed = test_def("test: no-seed\n");
    let second = test_def(
        r#"
test: second
seed:
  postgres:
    - sql: second-local
"#,
    );
    let flow = flow_def(
        r#"
flow: once
reset: once
stages:
  - test: first
  - test: no-seed
  - test: second
"#,
    );
    let tests = HashMap::from([
        (first.test.clone(), &first),
        (no_seed.test.clone(), &no_seed),
        (second.test.clone(), &second),
    ]);

    let outcome = runner.run_flow(&flow, &tests).await;

    assert!(outcome
        .results
        .iter()
        .all(|result| result.status == TestStatus::Passed));
    assert_eq!(&*state.events.lock().unwrap(), &["reset", "seed", "seed"]);
    assert_eq!(
        &*state.seed_docs.lock().unwrap(),
        &[
            json!([{"sql": "global"}, {"sql": "first-local"}]),
            json!([{"sql": "second-local"}]),
        ]
    );
}

#[tokio::test]
async fn reset_each_reapplies_global_seed_to_every_executable_stage() {
    let (runner, state) = runner(json!([{"sql": "global"}]), None).await;
    let first = test_def("test: first\n");
    let skipped = test_def("test: skipped\nskip: later\n");
    let second = test_def("test: second\n");
    let flow = flow_def(
        r#"
flow: each
reset: each
stages:
  - test: first
  - test: skipped
  - test: second
"#,
    );
    let tests = HashMap::from([
        (first.test.clone(), &first),
        (skipped.test.clone(), &skipped),
        (second.test.clone(), &second),
    ]);

    let outcome = runner.run_flow(&flow, &tests).await;

    assert_eq!(outcome.results[0].status, TestStatus::Passed);
    assert_eq!(outcome.results[1].status, TestStatus::Skipped);
    assert_eq!(outcome.results[2].status, TestStatus::Passed);
    assert_eq!(
        &*state.events.lock().unwrap(),
        &["reset", "seed", "reset", "seed"]
    );
    assert_eq!(state.seed_docs.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_reset_bound_seed_invalidates_a_reset_once_flow_even_on_continue() {
    let (runner, state) = runner(json!([{"sql": "global"}]), Some(1)).await;
    let first = test_def("test: first\n");
    let second = test_def(
        r#"
test: second
seed:
  postgres:
    - sql: must-not-run
"#,
    );
    let flow = flow_def(
        r#"
flow: broken baseline
reset: once
on_failure: continue
stages:
  - test: first
  - test: second
"#,
    );
    let tests = HashMap::from([(first.test.clone(), &first), (second.test.clone(), &second)]);

    let outcome = runner.run_flow(&flow, &tests).await;

    assert_eq!(outcome.results[0].status, TestStatus::Errored);
    assert_eq!(outcome.results[1].status, TestStatus::Skipped);
    assert!(outcome.results[1]
        .skip_reason
        .as_deref()
        .unwrap()
        .contains("reset-bound initialization failed"));
    assert_eq!(&*state.events.lock().unwrap(), &["reset", "seed"]);
    assert_eq!(state.seed_docs.lock().unwrap().len(), 1);
}
