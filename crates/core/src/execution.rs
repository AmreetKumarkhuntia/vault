//! Optional execution evidence. Legacy run-result serialization is unchanged.

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Map, Value};
use vault_mock::Session;

use crate::{TestResult, TestStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionStatus {
    Passed,
    Failed,
    Errored,
    Skipped,
    NotRun,
}

impl From<TestStatus> for ExecutionStatus {
    fn from(status: TestStatus) -> Self {
        match status {
            TestStatus::Passed => Self::Passed,
            TestStatus::Failed => Self::Failed,
            TestStatus::Errored => Self::Errored,
            TestStatus::Skipped => Self::Skipped,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ExecutionEvent {
    pub phase: String,
    pub subject: String,
    pub status: ExecutionStatus,
    pub start_offset_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub attempts: u32,
    pub step_index: Option<usize>,
    pub details: Value,
}

#[derive(Debug, Default, Serialize)]
pub struct TestExecution {
    pub events: Vec<ExecutionEvent>,
    /// Zero-based position in the containing flow, including skipped stages.
    pub flow_stage: Option<usize>,
    pub exports: Map<String, Value>,
    /// Transient credential discovery inputs; never embed them in reports.
    #[serde(skip)]
    pub redaction_sources: Vec<Value>,
}

/// Owned outside the timed lifecycle so cancellation cannot erase evidence.
pub(crate) struct ExecutionCollector {
    enabled: bool,
    collect_sources: bool,
    started: Instant,
    pub(crate) execution: TestExecution,
    open: Vec<(usize, Instant)>,
    session: Option<Arc<Session>>,
}

impl ExecutionCollector {
    pub(crate) fn new(enabled: bool) -> Self {
        Self::with_sources(enabled, enabled)
    }

    pub(crate) fn with_sources(enabled: bool, collect_sources: bool) -> Self {
        Self {
            enabled,
            collect_sources,
            started: Instant::now(),
            execution: TestExecution::default(),
            open: Vec::new(),
            session: None,
        }
    }

    pub(crate) fn collects_sources(&self) -> bool {
        self.collect_sources
    }

    pub(crate) fn source(&mut self, source: Value) {
        if self.collect_sources {
            self.execution.redaction_sources.push(source);
        }
    }

    pub(crate) fn request(&mut self, id: Option<usize>, request: Value) {
        if self.collect_sources {
            if id.is_some() {
                self.execution.redaction_sources.push(request.clone());
                self.detail(id, "request", request);
            } else {
                self.execution.redaction_sources.push(request);
            }
        }
    }

    pub(crate) fn start(
        &mut self,
        phase: &str,
        subject: &str,
        step_index: Option<usize>,
        attempts: u32,
        details: Value,
    ) -> Option<usize> {
        if !self.enabled {
            return None;
        }
        let id = self.execution.events.len();
        let now = Instant::now();
        self.execution.events.push(ExecutionEvent {
            phase: phase.into(),
            subject: subject.into(),
            status: ExecutionStatus::NotRun,
            start_offset_ms: Some(now.duration_since(self.started).as_millis() as u64),
            duration_ms: None,
            attempts,
            step_index,
            details,
        });
        self.open.push((id, now));
        Some(id)
    }

    pub(crate) fn detail(&mut self, id: Option<usize>, key: &str, value: Value) {
        if let Some(id) = id {
            if !self.execution.events[id].details.is_object() {
                self.execution.events[id].details = json!({});
            }
            self.execution.events[id].details[key] = value;
        }
    }

    pub(crate) fn complete(&mut self, id: Option<usize>, status: ExecutionStatus) {
        if let Some(id) = id {
            if let Some(position) = self.open.iter().position(|(open, _)| *open == id) {
                let (_, started) = self.open.remove(position);
                let event = &mut self.execution.events[id];
                event.status = status;
                event.duration_ms = Some(started.elapsed().as_millis() as u64);
            }
        }
    }

    pub(crate) fn checks(&mut self, id: Option<usize>, outcome: &vault_store::VerifyOutcome) {
        if id.is_some() {
            self.detail(
                id,
                "checks",
                serde_json::to_value(outcome).unwrap_or(Value::Null),
            );
        }
    }

    pub(crate) fn attempts(&mut self, id: Option<usize>, attempts: u32) {
        if let Some(id) = id {
            self.execution.events[id].attempts = attempts;
        }
    }

    pub(crate) fn session(&mut self, session: &Arc<Session>) {
        if self.collect_sources {
            self.session = Some(session.clone());
        }
    }

    pub(crate) fn not_run(&mut self, phase: &str, subject: &str, step_index: Option<usize>) {
        if !self.enabled
            || self.execution.events.iter().any(|event| {
                event.phase == phase && event.subject == subject && event.step_index == step_index
            })
        {
            return;
        }
        self.execution.events.push(ExecutionEvent {
            phase: phase.into(),
            subject: subject.into(),
            status: ExecutionStatus::NotRun,
            start_offset_ms: None,
            duration_ms: None,
            attempts: 0,
            step_index,
            details: json!({"reason": "test ended before this operation"}),
        });
    }

    pub(crate) fn finalize(&mut self, result: &TestResult) {
        if !self.collect_sources {
            return;
        }
        let reason = result.error.as_ref().or(result.skip_reason.as_ref());
        while let Some(&(id, _)) = self.open.last() {
            if let Some(reason) = reason {
                self.detail(Some(id), "error", json!(reason));
            }
            self.complete(Some(id), ExecutionStatus::Errored);
        }
        let recordings = self
            .session
            .take()
            .map(|session| {
                session
                    .log
                    .lock()
                    .iter()
                    .map(|exchange| exchange.to_report_value())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if self.enabled {
            self.execution
                .redaction_sources
                .extend(recordings.iter().cloned());
        } else {
            self.execution.redaction_sources.extend(recordings);
            return;
        }
        let id = self.start(
            "finalization",
            &result.name,
            None,
            0,
            json!({
                "test_status": result.status,
                "error": result.error,
                "skip_reason": result.skip_reason,
                "recorded_calls": recordings,
            }),
        );
        self.complete(id, ExecutionStatus::Passed);
    }
}
