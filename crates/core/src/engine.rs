use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use vault_dsl::{Defaults, FlowDef, FlowOnFailure, FlowReset, Step, TestDef};
use vault_mock::{MockServer, RecordedExchange, Session, SessionGuard};
use vault_store::matchers::{humantime_ms, MatchCtx};
use vault_store::{StateStore, VerifyOpts, VerifyOutcome};

use crate::execution::{ExecutionCollector, ExecutionStatus, TestExecution};
use crate::http::{execute_request_with_evidence, StepResponse};
use crate::result::*;
use crate::{assert_response, capture_value, CoreError, TemplateEngine};

pub enum PausePoint<'a> {
    AfterSeed,
    MocksArmed,
    AfterStep(&'a str),
    BeforeVerify,
}

pub struct PauseSnapshot<'a> {
    pub test: &'a str,
    pub last_response: Option<&'a StepResponse>,
    pub ctx: &'a Value,
    pub recordings: Vec<RecordedExchange>,
}

#[derive(PartialEq)]
pub enum GateDecision {
    Continue,
    AbortTest,
    AbortRun,
}

pub trait Gate: Send + Sync {
    fn pause(&self, point: &PausePoint, snap: &PauseSnapshot) -> GateDecision;
}

pub struct NoopGate;
impl Gate for NoopGate {
    fn pause(&self, _: &PausePoint, _: &PauseSnapshot) -> GateDecision {
        GateDecision::Continue
    }
}

pub struct TestRunner {
    pub client: reqwest::Client,
    pub target_base_url: String,
    pub defaults: Defaults,
    /// Suite-wide seed documents. At reset boundaries these are composed with
    /// test-local seed arrays and sent to each store as one seed operation.
    pub global_seed: IndexMap<String, Value>,
    pub stores: IndexMap<String, Arc<dyn StateStore>>,
    pub mock: Arc<MockServer>,
    pub gate: Arc<dyn Gate>,
    pub abort_run: std::sync::atomic::AtomicBool,
}

impl TestRunner {
    pub async fn run_test(
        &self,
        def: &TestDef,
        extra_vars: &IndexMap<String, Value>,
        do_reset: bool,
        flow_scope: Option<&Value>,
        flow_name: Option<&str>,
    ) -> TestResult {
        self.run_test_tracked(
            def, extra_vars, do_reset, flow_scope, flow_name, false, false,
        )
        .await
        .result
    }

    /// Execute a test with companion evidence without changing legacy results.
    pub async fn run_test_with_evidence(
        &self,
        def: &TestDef,
        extra_vars: &IndexMap<String, Value>,
        do_reset: bool,
        flow_scope: Option<&Value>,
        flow_name: Option<&str>,
    ) -> (TestResult, TestExecution) {
        self.run_test_for_reporting(def, extra_vars, do_reset, flow_scope, flow_name, true)
            .await
    }

    /// Capture credential sources for output masking, optionally with full trace.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_test_for_reporting(
        &self,
        def: &TestDef,
        extra_vars: &IndexMap<String, Value>,
        do_reset: bool,
        flow_scope: Option<&Value>,
        flow_name: Option<&str>,
        collect_details: bool,
    ) -> (TestResult, TestExecution) {
        let tracked = self
            .run_test_tracked(
                def,
                extra_vars,
                do_reset,
                flow_scope,
                flow_name,
                collect_details,
                true,
            )
            .await;
        (tracked.result, tracked.execution)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_test_tracked(
        &self,
        def: &TestDef,
        extra_vars: &IndexMap<String, Value>,
        do_reset: bool,
        flow_scope: Option<&Value>,
        flow_name: Option<&str>,
        collect_evidence: bool,
        collect_sources: bool,
    ) -> TrackedTestResult {
        let mut evidence = ExecutionCollector::with_sources(collect_evidence, collect_sources);
        if let Some(reason) = def.skip.reason() {
            let result = TestResult::skipped(&def.test, reason.clone());
            let event = evidence.start("test", &def.test, None, 0, json!({"reason": reason}));
            evidence.complete(event, ExecutionStatus::Skipped);
            self.record_remaining(def, do_reset, &mut evidence);
            evidence.finalize(&result);
            return TrackedTestResult {
                result,
                initialization: InitializationProgress::default(),
                execution: evidence.execution,
            };
        }
        let started = Instant::now();
        let mut initialization = InitializationProgress::default();
        let mut result = TestResult {
            name: def.test.clone(),
            status: TestStatus::Passed,
            skip_reason: None,
            error: None,
            steps: vec![],
            verify: VerifyOutcome::default(),
            seed_receipts: vec![],
            captures: Map::new(),
            recorded_calls: vec![],
            duration_ms: 0,
            flow: flow_name.map(String::from),
        };

        let outcome = tokio::time::timeout(
            def.timeout,
            self.lifecycle(
                def,
                extra_vars,
                do_reset,
                flow_scope,
                &mut result,
                &mut initialization,
                &mut evidence,
            ),
        )
        .await;

        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                result.status = TestStatus::Errored;
                result.error = Some(e.to_string());
            }
            Err(_) => {
                result.status = TestStatus::Errored;
                result.error = Some(format!("test timeout after {:?}", def.timeout));
            }
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        self.record_remaining(def, do_reset, &mut evidence);
        evidence.finalize(&result);
        TrackedTestResult {
            result,
            initialization,
            execution: evidence.execution,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn lifecycle(
        &self,
        def: &TestDef,
        extra_vars: &IndexMap<String, Value>,
        do_reset: bool,
        flow_scope: Option<&Value>,
        result: &mut TestResult,
        initialization: &mut InitializationProgress,
        evidence: &mut ExecutionCollector,
    ) -> Result<(), CoreError> {
        let preparation = evidence.start(
            "preparation",
            &def.test,
            None,
            0,
            json!({"reset_boundary": do_reset}),
        );
        let anchor_ms = now_ms();
        let match_ctx = MatchCtx {
            anchor_unix_ms: anchor_ms,
        };

        let base_urls: Map<String, Value> = def
            .mocks
            .iter()
            .map(|(name, dep)| {
                let prefix = dep.prefix.clone().unwrap_or_else(|| format!("/{name}"));
                (
                    name.clone(),
                    json!({"base_url": format!("{}{}", self.mock.base_url(), prefix)}),
                )
            })
            .collect();

        let mut engine = TemplateEngine::new(
            json!({
                "vars": {},
                "steps": {},
                "mock": base_urls,
                "env": std::env::vars().map(|(k, v)| (k, Value::String(v))).collect::<Map<String, Value>>(),
                "flow": flow_scope.cloned().unwrap_or_else(|| json!({})),
                "test": {"name": def.test},
            }),
            anchor_ms,
        );
        // Vars render in declaration order so later vars can reference earlier ones.
        for (k, v) in def.vars.iter().chain(extra_vars.iter()) {
            let rendered = engine.render_value(v)?;
            engine.set("vars", k, rendered);
        }

        // Rendering and structural composition happen before RESET. A bad
        // template or incompatible global/local document therefore cannot
        // consume a reset-once flow's pending isolation boundary.
        let seed_docs = self.render_seed_docs(def, &engine, do_reset)?;
        evidence.complete(preparation, ExecutionStatus::Passed);

        if do_reset {
            initialization.started = true;
            for (kind, store) in &self.stores {
                let spec = self
                    .defaults
                    .reset
                    .get(kind)
                    .cloned()
                    .unwrap_or(Value::Null);
                let event = evidence.start("reset", kind, None, 1, json!({}));
                store.reset(&spec).await?;
                evidence.complete(event, ExecutionStatus::Passed);
            }
        }

        for (kind, doc) in &seed_docs {
            let event = evidence.start(
                "seed",
                kind,
                None,
                1,
                json!({
                    "global": do_reset && self.global_seed.contains_key(kind),
                    "local": def.seed.contains_key(kind),
                    "entries": doc.as_array().map(Vec::len),
                }),
            );
            let store = self.store(kind)?;
            let receipt = store.seed(doc).await?;
            if event.is_some() {
                evidence.detail(event, "receipts", json!(receipt.entries));
            }
            evidence.complete(event, ExecutionStatus::Passed);
            result.seed_receipts.extend(receipt.entries);
        }

        if do_reset {
            initialization.completed = true;
        }

        let watch_snapshot = if def.watch.is_empty() {
            None
        } else {
            let event = evidence.start(
                "watch_snapshot",
                "postgres",
                None,
                1,
                json!({"tables": def.watch}),
            );
            let store = self.store("postgres")?;
            let doc = json!(def.watch);
            let snapshot = store.snapshot(&doc).await?;
            evidence.complete(event, ExecutionStatus::Passed);
            Some((store.clone(), doc.clone(), snapshot))
        };

        if self.gate_pause(&PausePoint::AfterSeed, def, None, &engine, None)
            == GateDecision::AbortTest
        {
            return Err(CoreError::Harness("aborted at seed".into()));
        }

        let mocks = evidence.start(
            "mock_arm",
            &def.test,
            None,
            1,
            json!({"dependencies": def.mocks.keys().collect::<Vec<_>>()}),
        );
        let rendered_mocks = render_mocks(&engine, def)?;
        let guard = self.mock.arm(Session::new(
            &def.test,
            &rendered_mocks,
            self.defaults.mock.unmatched.clone(),
        ));

        evidence.session(guard.session());
        evidence.complete(mocks, ExecutionStatus::Passed);

        if self.gate_pause(&PausePoint::MocksArmed, def, None, &engine, Some(&guard))
            == GateDecision::AbortTest
        {
            return Err(CoreError::Harness("aborted at mocks".into()));
        }

        let mut last_response: Option<StepResponse> = None;
        for (step_index, step) in def.steps.iter().enumerate() {
            let (step_result, resp) = self
                .run_step(step, &mut engine, &match_ctx, step_index, evidence)
                .await?;
            let failed = step_result.status == TestStatus::Failed;
            result.steps.push(step_result);
            last_response = resp;

            let decision = self.gate_pause(
                &PausePoint::AfterStep(&step.name),
                def,
                last_response.as_ref(),
                &engine,
                Some(&guard),
            );
            if decision == GateDecision::AbortTest {
                return Err(CoreError::Harness(format!(
                    "aborted at step `{}`",
                    step.name
                )));
            }
            if failed {
                result.status = TestStatus::Failed;
                // Fail fast BETWEEN steps: captures downstream would be garbage.
                break;
            }
        }

        for (name, _) in def.steps.iter().flat_map(|s| &s.capture) {
            if let Some(v) = engine.context().get(name) {
                result.captures.insert(name.clone(), v.clone());
            }
        }

        if self.gate_pause(
            &PausePoint::BeforeVerify,
            def,
            last_response.as_ref(),
            &engine,
            Some(&guard),
        ) == GateDecision::AbortTest
        {
            return Err(CoreError::Harness("aborted before verify".into()));
        }

        // End-state verification runs even when a step FAILED (not ERRORED):
        // "response was wrong AND here's what hit the DB" is the debugging gold.
        let mut verify = VerifyOutcome::default();
        for (kind, doc) in &def.verify.stores {
            let event = evidence.start("verify_store", kind, None, 0, json!({}));
            let store = self.store(kind)?;
            let rendered = engine.render_value(doc)?;
            let outcome = self
                .poll_verify(store.as_ref(), &rendered, anchor_ms, evidence, event)
                .await?;
            evidence.checks(event, &outcome);
            evidence.complete(
                event,
                if outcome.passed() {
                    ExecutionStatus::Passed
                } else {
                    ExecutionStatus::Failed
                },
            );
            verify.merge(outcome);
        }

        if let Some((store, doc, before)) = watch_snapshot {
            let event = evidence.start(
                "watch_diff",
                "postgres",
                None,
                1,
                json!({"tables": def.watch}),
            );
            let outcome = store.diff_snapshot(&doc, &before).await?;
            evidence.checks(event, &outcome);
            evidence.complete(
                event,
                if outcome.passed() {
                    ExecutionStatus::Passed
                } else {
                    ExecutionStatus::Failed
                },
            );
            verify.merge(outcome);
        }

        if !def.verify.calls.is_empty() || !def.mocks.is_empty() {
            let quiet = evidence.start("mock_quiet", &def.test, None, 1, json!({}));
            self.wait_for_mock_quiet(&guard).await;
            evidence.complete(quiet, ExecutionStatus::Passed);
            let event = evidence.start("verify_calls", &def.test, None, 1, json!({}));
            let report = guard.drain();
            let unexpected = def
                .verify
                .unexpected
                .clone()
                .unwrap_or(vault_dsl::UnmatchedPolicy::Fail);
            let rendered_calls: Vec<vault_dsl::CallExpect> = def
                .verify
                .calls
                .iter()
                .map(|c| {
                    let v = engine.render_value(&serde_json::to_value(c).unwrap())?;
                    serde_json::from_value(v)
                        .map_err(|e| CoreError::Harness(format!("verify.calls: {e}")))
                })
                .collect::<Result<_, CoreError>>()?;
            let call_outcome = vault_mock::verify_calls(
                &report,
                &rendered_calls,
                &def.verify.ordered,
                &unexpected,
                &match_ctx,
            );
            evidence.checks(event, &call_outcome);
            evidence.complete(
                event,
                if call_outcome.passed() {
                    ExecutionStatus::Passed
                } else {
                    ExecutionStatus::Failed
                },
            );
            verify.merge(call_outcome);
            if result.status != TestStatus::Passed || !verify.passed() {
                result.recorded_calls = report
                    .recordings
                    .iter()
                    .map(|r| r.to_report_value())
                    .collect();
            }
        }

        if !verify.passed() {
            result.status = result.status.worst(TestStatus::Failed);
        }
        result.verify = verify;
        Ok(())
    }

    async fn run_step(
        &self,
        step: &Step,
        engine: &mut TemplateEngine,
        match_ctx: &MatchCtx,
        step_index: usize,
        evidence: &mut ExecutionCollector,
    ) -> Result<(StepResult, Option<StepResponse>), CoreError> {
        let step_event = evidence.start("step", &step.name, Some(step_index), 0, json!({}));
        let deadline = step.repeat.as_ref().map(|r| Instant::now() + r.timeout);
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            evidence.attempts(step_event, attempts);
            let attempt = evidence.start(
                "http_attempt",
                &step.name,
                Some(step_index),
                attempts,
                json!({}),
            );
            let resp = execute_request_with_evidence(
                &self.client,
                &self.target_base_url,
                &step.request,
                engine,
                self.defaults.request.timeout,
                &self.defaults.request.headers,
                evidence,
                attempt,
            )
            .await?;

            let checks = match &step.expect {
                Some(spec) => {
                    let rendered: vault_dsl::ExpectSpec = serde_json::from_value(
                        engine.render_value(&serde_json::to_value(spec).unwrap())?,
                    )
                    .map_err(|e| CoreError::Harness(format!("expect: {e}")))?;
                    assert_response(&step.name, &rendered, &resp, match_ctx)
                }
                None => VerifyOutcome::default(),
            };

            let passed = checks.passed();
            evidence.checks(attempt, &checks);
            evidence.complete(
                attempt,
                if passed {
                    ExecutionStatus::Passed
                } else {
                    ExecutionStatus::Failed
                },
            );
            if !passed {
                if let Some(d) = deadline {
                    if Instant::now() < d {
                        tokio::time::sleep(step.repeat.as_ref().unwrap().every).await;
                        continue;
                    }
                }
            }

            if passed {
                for (name, spec) in &step.capture {
                    let capture = evidence.start("capture", name, Some(step_index), 1, json!({}));
                    let value = capture_value(name, spec, &resp)?;
                    if evidence.collects_sources() {
                        evidence.source(json!({name: value}));
                    }
                    if capture.is_some() {
                        evidence.detail(capture, "value", value.clone());
                    }
                    evidence.complete(capture, ExecutionStatus::Passed);
                    engine.set_flat(name, value.clone());
                    let captures_path = engine
                        .context()
                        .get("steps")
                        .and_then(|s| s.get(&step.name))
                        .is_some();
                    if !captures_path {
                        engine.set("steps", &step.name, json!({"captures": {}}));
                    }
                    if let Some(caps) = engine
                        .context()
                        .get("steps")
                        .and_then(|s| s.get(&step.name))
                        .and_then(|s| s.get("captures"))
                        .cloned()
                    {
                        let mut caps = caps;
                        caps.as_object_mut().unwrap().insert(name.clone(), value);
                        engine.set("steps", &step.name, json!({"captures": caps}));
                    }
                }
            }

            let step_result = StepResult {
                name: step.name.clone(),
                status: if passed {
                    TestStatus::Passed
                } else {
                    TestStatus::Failed
                },
                response: Some(ResponseSummary {
                    status: resp.status,
                    elapsed_ms: resp.elapsed.as_millis() as u64,
                    body: if resp.body_json.is_null() {
                        json!(truncate(&resp.body, 2000))
                    } else {
                        resp.body_json.clone()
                    },
                }),
                checks,
                attempts,
            };
            evidence.complete(step_event, step_result.status.into());
            return Ok((step_result, Some(resp)));
        }
    }

    /// `eventually:` drives repeated single-shot store verifies. After the
    /// first full pass the result must survive a settle re-check, otherwise a
    /// negative assertion could pass an instant before async work lands.
    async fn poll_verify(
        &self,
        store: &dyn StateStore,
        doc: &Value,
        anchor_ms: i64,
        evidence: &mut ExecutionCollector,
        event: Option<usize>,
    ) -> Result<VerifyOutcome, CoreError> {
        let settle = self.defaults.verify.settle;
        let interval = self.defaults.verify.poll_interval;
        let deadline_ms = max_eventually_ms(doc);
        let opts = VerifyOpts {
            anchor_unix_ms: anchor_ms,
            settle,
        };
        let started = Instant::now();
        let deadline = started + Duration::from_millis(deadline_ms);
        let mut attempts = 0u32;

        loop {
            attempts += 1;
            evidence.attempts(event, attempts);
            let poll = evidence.start(
                "verify_poll",
                store.alias(),
                None,
                attempts,
                json!({"settle_confirmation": false}),
            );
            let outcome = store.verify(doc, &opts).await?;
            evidence.checks(poll, &outcome);
            evidence.complete(
                poll,
                if outcome.passed() {
                    ExecutionStatus::Passed
                } else {
                    ExecutionStatus::Failed
                },
            );
            if outcome.passed() {
                if deadline_ms == 0 || settle.is_zero() {
                    return Ok(stamp(outcome, attempts, started));
                }
                tokio::time::sleep(settle).await;
                attempts += 1;
                evidence.attempts(event, attempts);
                let poll = evidence.start(
                    "verify_poll",
                    store.alias(),
                    None,
                    attempts,
                    json!({"settle_confirmation": true}),
                );
                let confirm = store.verify(doc, &opts).await?;
                evidence.checks(poll, &confirm);
                evidence.complete(
                    poll,
                    if confirm.passed() {
                        ExecutionStatus::Passed
                    } else {
                        ExecutionStatus::Failed
                    },
                );
                if confirm.passed() {
                    return Ok(stamp(confirm, attempts, started));
                }
            }
            if Instant::now() >= deadline {
                return Ok(stamp(outcome, attempts, started));
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn wait_for_mock_quiet(&self, guard: &SessionGuard) {
        let settle = self.defaults.verify.settle.as_millis() as u64;
        let budget = Instant::now() + Duration::from_millis((settle * 4).max(2000));
        loop {
            let last = guard.last_recorded_at_ms();
            let now = now_ms() as u64;
            if last == 0 || now.saturating_sub(last) >= settle || Instant::now() >= budget {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn gate_pause(
        &self,
        point: &PausePoint,
        def: &TestDef,
        last_response: Option<&StepResponse>,
        engine: &TemplateEngine,
        guard: Option<&SessionGuard>,
    ) -> GateDecision {
        let snap = PauseSnapshot {
            test: &def.test,
            last_response,
            ctx: engine.context(),
            recordings: guard.map(|g| g.drain().recordings).unwrap_or_default(),
        };
        let decision = self.gate.pause(point, &snap);
        if decision == GateDecision::AbortRun {
            self.abort_run
                .store(true, std::sync::atomic::Ordering::SeqCst);
            return GateDecision::AbortTest;
        }
        decision
    }

    fn store(&self, kind: &str) -> Result<&Arc<dyn StateStore>, CoreError> {
        self.stores.get(kind).ok_or_else(|| {
            CoreError::Harness(format!(
                "store `{kind}` is not configured for this environment"
            ))
        })
    }

    fn render_seed_docs(
        &self,
        def: &TestDef,
        engine: &TemplateEngine,
        include_global: bool,
    ) -> Result<IndexMap<String, Value>, CoreError> {
        let mut docs = IndexMap::new();

        if include_global {
            for (kind, doc) in &self.global_seed {
                docs.insert(kind.clone(), engine.render_value(doc)?);
            }
        }

        for (kind, doc) in &def.seed {
            let local = engine.render_value(doc)?;
            match docs.get_mut(kind) {
                Some(global) => {
                    let global_entries = global.as_array_mut().ok_or_else(|| {
                        CoreError::Harness(format!(
                            "global and test seed documents for store `{kind}` must both be arrays"
                        ))
                    })?;
                    let local_entries = local.as_array().ok_or_else(|| {
                        CoreError::Harness(format!(
                            "global and test seed documents for store `{kind}` must both be arrays"
                        ))
                    })?;
                    global_entries.extend(local_entries.iter().cloned());
                }
                None => {
                    docs.insert(kind.clone(), local);
                }
            }
        }

        Ok(docs)
    }

    fn record_remaining(&self, def: &TestDef, do_reset: bool, evidence: &mut ExecutionCollector) {
        evidence.not_run("preparation", &def.test, None);
        if do_reset {
            for kind in self.stores.keys() {
                evidence.not_run("reset", kind, None);
            }
        }
        let mut seed_kinds: Vec<&String> = def.seed.keys().collect();
        if do_reset {
            seed_kinds.extend(self.global_seed.keys());
        }
        for kind in seed_kinds {
            evidence.not_run("seed", kind, None);
        }
        if !def.watch.is_empty() {
            evidence.not_run("watch_snapshot", "postgres", None);
        }
        evidence.not_run("mock_arm", &def.test, None);
        for (index, step) in def.steps.iter().enumerate() {
            evidence.not_run("step", &step.name, Some(index));
        }
        for kind in def.verify.stores.keys() {
            evidence.not_run("verify_store", kind, None);
        }
        if !def.watch.is_empty() {
            evidence.not_run("watch_diff", "postgres", None);
        }
        if !def.mocks.is_empty() || !def.verify.calls.is_empty() {
            evidence.not_run("mock_quiet", &def.test, None);
            evidence.not_run("verify_calls", &def.test, None);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_evidence(
        &self,
        result: &TestResult,
        def: Option<&TestDef>,
        do_reset: bool,
        index: usize,
        collect_details: bool,
        collect_sources: bool,
    ) -> TestExecution {
        let mut evidence = ExecutionCollector::with_sources(collect_details, collect_sources);
        let event = evidence.start(
            "stage",
            &result.name,
            None,
            0,
            json!({
                "reason": result.error.as_ref().or(result.skip_reason.as_ref()),
            }),
        );
        evidence.complete(event, result.status.into());
        if let Some(def) = def {
            self.record_remaining(def, do_reset, &mut evidence);
        }
        evidence.finalize(result);
        evidence.execution.flow_stage = Some(index);
        evidence.execution
    }

    pub async fn run_flow(&self, flow: &FlowDef, tests: &HashMap<String, &TestDef>) -> FlowOutcome {
        self.run_flow_tracked(flow, tests, false, false).await.0
    }

    /// Evidence stays aligned with every stage, including skipped/error stages.
    pub async fn run_flow_with_evidence(
        &self,
        flow: &FlowDef,
        tests: &HashMap<String, &TestDef>,
    ) -> (FlowOutcome, Vec<TestExecution>) {
        self.run_flow_for_reporting(flow, tests, true).await
    }

    /// Collect credential inputs for every stage; detailed tracing is optional.
    pub async fn run_flow_for_reporting(
        &self,
        flow: &FlowDef,
        tests: &HashMap<String, &TestDef>,
        collect_details: bool,
    ) -> (FlowOutcome, Vec<TestExecution>) {
        self.run_flow_tracked(flow, tests, collect_details, true)
            .await
    }

    async fn run_flow_tracked(
        &self,
        flow: &FlowDef,
        tests: &HashMap<String, &TestDef>,
        collect_evidence: bool,
        collect_sources: bool,
    ) -> (FlowOutcome, Vec<TestExecution>) {
        let mut results = Vec::new();
        let mut executions = Vec::new();
        let mut flow_scope = Map::new();
        let mut chain_broken = false;
        let mut reset_pending = flow.reset == FlowReset::Once;
        let mut isolation_invalid = false;

        for (stage_index, stage) in flow.stages.iter().enumerate() {
            let do_reset = flow.reset == FlowReset::Each || reset_pending;
            let Some(def) = tests.get(&stage.test) else {
                let result =
                    TestResult::skipped(&stage.test, "unknown test referenced by flow".into());
                if collect_sources {
                    executions.push(self.stage_evidence(
                        &result,
                        None,
                        do_reset,
                        stage_index,
                        collect_evidence,
                        collect_sources,
                    ));
                }
                results.push(result);
                continue;
            };
            if isolation_invalid {
                let mut r = TestResult::skipped(
                    &stage.test,
                    format!(
                        "reset-bound initialization failed earlier in flow `{}`",
                        flow.flow
                    ),
                );
                r.flow = Some(flow.flow.clone());
                if collect_sources {
                    executions.push(self.stage_evidence(
                        &r,
                        Some(def),
                        do_reset,
                        stage_index,
                        collect_evidence,
                        collect_sources,
                    ));
                }
                results.push(r);
                continue;
            }
            if chain_broken && flow.on_failure == FlowOnFailure::SkipRest {
                let mut r = TestResult::skipped(
                    &stage.test,
                    format!("dependency failed earlier in flow `{}`", flow.flow),
                );
                r.flow = Some(flow.flow.clone());
                if collect_sources {
                    executions.push(self.stage_evidence(
                        &r,
                        Some(def),
                        do_reset,
                        stage_index,
                        collect_evidence,
                        collect_sources,
                    ));
                }
                results.push(r);
                continue;
            }

            let scope = Value::Object(flow_scope.clone());
            let with_engine = TemplateEngine::new(json!({"flow": scope.clone()}), now_ms());
            let mut extra = IndexMap::new();
            let mut with_error = None;
            for (k, v) in &stage.with {
                match with_engine.render_value(v) {
                    Ok(rendered) => {
                        extra.insert(k.clone(), rendered);
                    }
                    Err(e) => with_error = Some(e.to_string()),
                }
            }
            if let Some(e) = with_error {
                let mut r = TestResult::skipped(&stage.test, format!("with: render failed: {e}"));
                r.status = TestStatus::Errored;
                if collect_sources {
                    executions.push(self.stage_evidence(
                        &r,
                        Some(def),
                        do_reset,
                        stage_index,
                        collect_evidence,
                        collect_sources,
                    ));
                }
                results.push(r);
                chain_broken = true;
                continue;
            }

            let tracked = self
                .run_test_tracked(
                    def,
                    &extra,
                    do_reset,
                    Some(&scope),
                    Some(&flow.flow),
                    collect_evidence,
                    collect_sources,
                )
                .await;
            let mut result = tracked.result;
            let mut execution = tracked.execution;
            execution.flow_stage = Some(stage_index);

            if flow.reset == FlowReset::Once && do_reset {
                if tracked.initialization.completed {
                    reset_pending = false;
                } else if tracked.initialization.started {
                    reset_pending = false;
                    isolation_invalid = true;
                }
            }

            if matches!(result.status, TestStatus::Failed | TestStatus::Errored) {
                chain_broken = true;
            }
            for export in &stage.export {
                if let Some(v) = result.captures.get(export) {
                    flow_scope.insert(export.clone(), v.clone());
                    if collect_evidence {
                        execution.exports.insert(export.clone(), v.clone());
                    }
                }
            }
            result.flow = Some(flow.flow.clone());
            results.push(result);
            if collect_sources {
                executions.push(execution);
            }
        }

        (
            FlowOutcome {
                name: flow.flow.clone(),
                results,
            },
            executions,
        )
    }
}

#[derive(Default)]
struct InitializationProgress {
    started: bool,
    completed: bool,
}

struct TrackedTestResult {
    result: TestResult,
    initialization: InitializationProgress,
    execution: TestExecution,
}

pub struct FlowOutcome {
    pub name: String,
    pub results: Vec<TestResult>,
}

fn render_mocks(
    engine: &TemplateEngine,
    def: &TestDef,
) -> Result<IndexMap<String, vault_dsl::MockDep>, CoreError> {
    let mut out = IndexMap::new();
    for (name, dep) in &def.mocks {
        let raw = serde_json::to_value(dep)
            .map_err(|e| CoreError::Harness(format!("mocks.{name}: {e}")))?;
        // Serve-time namespaces stay for the mock server to render per request.
        let rendered = render_except_request_ns(engine, &raw)?;
        let dep: vault_dsl::MockDep = serde_json::from_value(rendered)
            .map_err(|e| CoreError::Harness(format!("mocks.{name}: {e}")))?;
        out.insert(name.clone(), dep);
    }
    Ok(out)
}

fn render_except_request_ns(engine: &TemplateEngine, v: &Value) -> Result<Value, CoreError> {
    match v {
        Value::String(s) if s.contains("{{") => {
            if s.contains("request.") || s.contains("uuid()") || s.contains("now()") {
                Ok(v.clone())
            } else {
                engine.render_value(v)
            }
        }
        Value::Array(items) => Ok(Value::Array(
            items
                .iter()
                .map(|i| render_except_request_ns(engine, i))
                .collect::<Result<_, _>>()?,
        )),
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                out.insert(k.clone(), render_except_request_ns(engine, val)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn max_eventually_ms(doc: &Value) -> u64 {
    match doc {
        Value::Array(entries) => entries.iter().map(max_eventually_ms).max().unwrap_or(0),
        Value::Object(m) => m
            .get("eventually")
            .and_then(|e| match e {
                Value::String(s) => humantime_ms(s),
                Value::Number(n) => n.as_i64().map(|s| s * 1000),
                _ => None,
            })
            .map(|ms| ms.max(0) as u64)
            .unwrap_or(0),
        _ => 0,
    }
}

fn stamp(mut outcome: VerifyOutcome, attempts: u32, started: Instant) -> VerifyOutcome {
    let elapsed = started.elapsed().as_millis() as u64;
    for check in &mut outcome.checks {
        if let vault_store::CheckResult::Fail(f) = check {
            f.attempts = attempts;
            f.elapsed_ms = elapsed;
        }
    }
    outcome
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..s.floor_char_boundary(n)])
    }
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
