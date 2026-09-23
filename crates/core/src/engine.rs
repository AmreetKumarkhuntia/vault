use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use vault_dsl::{Defaults, FlowDef, FlowOnFailure, FlowReset, Step, TestDef};
use vault_mock::{MockServer, RecordedExchange, Session, SessionGuard};
use vault_store::matchers::{humantime_ms, MatchCtx};
use vault_store::{StateStore, VerifyOpts, VerifyOutcome};

use crate::http::{execute_request, StepResponse};
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
        if let Some(reason) = def.skip.reason() {
            return TestResult::skipped(&def.test, reason);
        }
        let started = Instant::now();
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
            self.lifecycle(def, extra_vars, do_reset, flow_scope, &mut result),
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
        result
    }

    async fn lifecycle(
        &self,
        def: &TestDef,
        extra_vars: &IndexMap<String, Value>,
        do_reset: bool,
        flow_scope: Option<&Value>,
        result: &mut TestResult,
    ) -> Result<(), CoreError> {
        let anchor_ms = now_ms();
        let match_ctx = MatchCtx { anchor_unix_ms: anchor_ms };

        if do_reset {
            for (kind, store) in &self.stores {
                let spec = self.defaults.reset.get(kind).cloned().unwrap_or(Value::Null);
                store.reset(&spec).await?;
            }
        }

        let base_urls: Map<String, Value> = def
            .mocks
            .iter()
            .map(|(name, dep)| {
                let prefix = dep.prefix.clone().unwrap_or_else(|| format!("/{name}"));
                (name.clone(), json!({"base_url": format!("{}{}", self.mock.base_url(), prefix)}))
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

        for (kind, doc) in &def.seed {
            let store = self.store(kind)?;
            let rendered = engine.render_value(doc)?;
            let receipt = store.seed(&rendered).await?;
            result.seed_receipts.extend(receipt.entries);
        }

        let watch_snapshot = if def.watch.is_empty() {
            None
        } else {
            let store = self.store("postgres")?;
            let doc = json!(def.watch);
            Some((store.clone(), doc.clone(), store.snapshot(&doc).await?))
        };

        if self.gate_pause(&PausePoint::AfterSeed, def, None, &engine, None) == GateDecision::AbortTest {
            return Err(CoreError::Harness("aborted at seed".into()));
        }

        let rendered_mocks = render_mocks(&engine, def)?;
        let guard = self
            .mock
            .arm(Session::new(&def.test, &rendered_mocks, self.defaults.mock.unmatched.clone()));

        if self.gate_pause(&PausePoint::MocksArmed, def, None, &engine, Some(&guard))
            == GateDecision::AbortTest
        {
            return Err(CoreError::Harness("aborted at mocks".into()));
        }

        let mut last_response: Option<StepResponse> = None;
        for step in &def.steps {
            let (step_result, resp) = self.run_step(step, &mut engine, &match_ctx).await?;
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
                return Err(CoreError::Harness(format!("aborted at step `{}`", step.name)));
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

        if self.gate_pause(&PausePoint::BeforeVerify, def, last_response.as_ref(), &engine, Some(&guard))
            == GateDecision::AbortTest
        {
            return Err(CoreError::Harness("aborted before verify".into()));
        }

        // End-state verification runs even when a step FAILED (not ERRORED):
        // "response was wrong AND here's what hit the DB" is the debugging gold.
        let mut verify = VerifyOutcome::default();
        for (kind, doc) in &def.verify.stores {
            let store = self.store(kind)?;
            let rendered = engine.render_value(doc)?;
            let outcome = self
                .poll_verify(store.as_ref(), &rendered, anchor_ms)
                .await?;
            verify.merge(outcome);
        }

        if let Some((store, doc, before)) = watch_snapshot {
            verify.merge(store.diff_snapshot(&doc, &before).await?);
        }

        if !def.verify.calls.is_empty()
            || !def.mocks.is_empty()
        {
            self.wait_for_mock_quiet(&guard).await;
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
            verify.merge(vault_mock::verify_calls(
                &report,
                &rendered_calls,
                &def.verify.ordered,
                &unexpected,
                &match_ctx,
            ));
            if result.status != TestStatus::Passed || !verify.passed() {
                result.recorded_calls =
                    report.recordings.iter().map(|r| r.to_report_value()).collect();
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
    ) -> Result<(StepResult, Option<StepResponse>), CoreError> {
        let deadline = step.repeat.as_ref().map(|r| Instant::now() + r.timeout);
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            let resp = execute_request(
                &self.client,
                &self.target_base_url,
                &step.request,
                engine,
                self.defaults.request.timeout,
                &self.defaults.request.headers,
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
                    let value = capture_value(name, spec, &resp)?;
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
                status: if passed { TestStatus::Passed } else { TestStatus::Failed },
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
    ) -> Result<VerifyOutcome, CoreError> {
        let settle = self.defaults.verify.settle;
        let interval = self.defaults.verify.poll_interval;
        let deadline_ms = max_eventually_ms(doc);
        let opts = VerifyOpts { anchor_unix_ms: anchor_ms, settle };
        let started = Instant::now();
        let deadline = started + Duration::from_millis(deadline_ms);
        let mut attempts = 0u32;

        loop {
            attempts += 1;
            let outcome = store.verify(doc, &opts).await?;
            if outcome.passed() {
                if deadline_ms == 0 || settle.is_zero() {
                    return Ok(stamp(outcome, attempts, started));
                }
                tokio::time::sleep(settle).await;
                attempts += 1;
                let confirm = store.verify(doc, &opts).await?;
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
            self.abort_run.store(true, std::sync::atomic::Ordering::SeqCst);
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

    pub async fn run_flow(
        &self,
        flow: &FlowDef,
        tests: &HashMap<String, &TestDef>,
    ) -> FlowOutcome {
        let mut results = Vec::new();
        let mut flow_scope = Map::new();
        let mut chain_broken = false;

        for (i, stage) in flow.stages.iter().enumerate() {
            let Some(def) = tests.get(&stage.test) else {
                results.push(TestResult::skipped(
                    &stage.test,
                    "unknown test referenced by flow".into(),
                ));
                continue;
            };
            if chain_broken && flow.on_failure == FlowOnFailure::SkipRest {
                let mut r = TestResult::skipped(
                    &stage.test,
                    format!("dependency failed earlier in flow `{}`", flow.flow),
                );
                r.flow = Some(flow.flow.clone());
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
                results.push(r);
                chain_broken = true;
                continue;
            }

            let do_reset = flow.reset == FlowReset::Each || i == 0;
            let mut result = self
                .run_test(def, &extra, do_reset, Some(&scope), Some(&flow.flow))
                .await;
            if result.status != TestStatus::Passed {
                chain_broken = true;
            }
            for export in &stage.export {
                if let Some(v) = result.captures.get(export) {
                    flow_scope.insert(export.clone(), v.clone());
                }
            }
            result.flow = Some(flow.flow.clone());
            results.push(result);
        }

        FlowOutcome { name: flow.flow.clone(), results }
    }
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
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}
