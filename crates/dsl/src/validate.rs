use std::collections::HashSet;

use serde_json::Value as Json;

use crate::{Suite, TestDef};

#[derive(Debug)]
pub struct ValidationIssue {
    pub origin: String,
    pub message: String,
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.origin, self.message)
    }
}

/// Static, cross-file validation. Store-doc validation is driver-owned and
/// runs separately (the CLI passes each store block to its driver's
/// `validate`); this covers everything structural the DSL itself knows.
pub fn validate_suite(suite: &Suite) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    // Unique test names.
    let mut names = HashSet::new();
    for t in &suite.tests {
        if !names.insert(t.def.test.clone()) {
            issues.push(ValidationIssue {
                origin: t.path.display().to_string(),
                message: format!("duplicate test name `{}`", t.def.test),
            });
        }
    }

    for t in &suite.tests {
        validate_test(&t.def, &t.path.display().to_string(), &mut issues);
    }

    // Flows: stages reference existing tests; flow.* refs are exported earlier;
    // exports name captures that actually exist in the referenced test.
    let mut flow_names = HashSet::new();
    for f in &suite.flows {
        let origin = f.path.display().to_string();
        if !flow_names.insert(f.def.flow.clone()) {
            issues.push(ValidationIssue {
                origin: origin.clone(),
                message: format!("duplicate flow name `{}`", f.def.flow),
            });
        }
        if f.def.stages.is_empty() {
            issues.push(ValidationIssue {
                origin: origin.clone(),
                message: "flow has no stages; it would run nothing and pass".into(),
            });
        }
        let mut exported: HashSet<String> = HashSet::new();
        let mut stage_names = HashSet::new();
        for (i, stage) in f.def.stages.iter().enumerate() {
            if !stage_names.insert(stage.test.clone()) {
                issues.push(ValidationIssue {
                    origin: origin.clone(),
                    message: format!("stages[{i}]: test `{}` appears twice in flow", stage.test),
                });
            }
            let Some(test) = suite.tests.iter().find(|t| t.def.test == stage.test) else {
                issues.push(ValidationIssue {
                    origin: origin.clone(),
                    message: format!("stages[{i}]: unknown test `{}`", stage.test),
                });
                continue;
            };
            // flow.* references in `with:` must be exported by an EARLIER stage.
            for (k, v) in &stage.with {
                if !test.def.vars.contains_key(k) {
                    issues.push(ValidationIssue {
                        origin: origin.clone(),
                        message: format!(
                            "stages[{i}].with.{k}: test `{}` declares no var `{k}`",
                            stage.test
                        ),
                    });
                }
                for var in find_flow_refs(&serde_json::to_string(v).unwrap_or_default()) {
                    // `flow.uid` and `flow.name` are always present.
                    if !matches!(var.as_str(), "uid" | "name") && !exported.contains(&var) {
                        issues.push(ValidationIssue {
                            origin: origin.clone(),
                            message: format!(
                                "stages[{i}].with.{k}: `flow.{var}` is not exported by any earlier stage"
                            ),
                        });
                    }
                }
            }
            // exports must exist as captures somewhere in the test.
            let captures: HashSet<&str> = test
                .def
                .steps
                .iter()
                .flat_map(|s| s.capture.keys())
                .map(String::as_str)
                .collect();
            for e in &stage.export {
                if !captures.contains(e.as_str()) {
                    issues.push(ValidationIssue {
                        origin: origin.clone(),
                        message: format!(
                            "stages[{i}]: export `{e}` is not captured by any step of `{}`",
                            stage.test
                        ),
                    });
                }
                exported.insert(e.clone());
            }
        }
    }

    issues
}

fn validate_test(def: &TestDef, origin: &str, issues: &mut Vec<ValidationIssue>) {
    // Unique step names.
    let mut step_names: Vec<&str> = Vec::new();
    for (i, step) in def.steps.iter().enumerate() {
        if step_names.contains(&step.name.as_str()) {
            issues.push(ValidationIssue {
                origin: origin.into(),
                message: format!("steps[{i}]: duplicate step name `{}`", step.name),
            });
        }
        // A request needs exactly one of path/url.
        match (&step.request.path, &step.request.url) {
            (None, None) => issues.push(ValidationIssue {
                origin: origin.into(),
                message: format!("steps[{i}] `{}`: request needs `path` or `url`", step.name),
            }),
            (Some(_), Some(_)) => issues.push(ValidationIssue {
                origin: origin.into(),
                message: format!(
                    "steps[{i}] `{}`: request has both `path` and `url`",
                    step.name
                ),
            }),
            _ => {}
        }
        // Each capture needs exactly one source.
        for (cname, cap) in &step.capture {
            let sources = [
                cap.jsonpath.is_some(),
                cap.header.is_some(),
                cap.status.is_some(),
                cap.body.is_some(),
                cap.regex.is_some(),
            ]
            .iter()
            .filter(|b| **b)
            .count();
            if sources != 1 {
                issues.push(ValidationIssue {
                    origin: origin.into(),
                    message: format!(
                        "steps[{i}] `{}`: capture `{cname}` needs exactly one source (jsonpath/header/status/body/regex)",
                        step.name
                    ),
                });
            }
        }
        // steps.<name>.captures.* references must point to EARLIER steps.
        let rendered = serde_json::to_string(&step.request).unwrap_or_default();
        for referenced in find_step_refs(&rendered) {
            if !step_names.contains(&referenced.as_str()) {
                issues.push(ValidationIssue {
                    origin: origin.into(),
                    message: format!(
                        "steps[{i}] `{}`: references `steps.{referenced}.captures` but `{referenced}` is not an earlier step",
                        step.name
                    ),
                });
            }
        }
        step_names.push(&step.name);
    }

    // verify.calls must reference declared mock dependencies.
    for (i, call) in def.verify.calls.iter().enumerate() {
        if !def.mocks.contains_key(&call.dependency) {
            issues.push(ValidationIssue {
                origin: origin.into(),
                message: format!(
                    "verify.calls[{i}]: dependency `{}` is not declared under mocks:",
                    call.dependency
                ),
            });
        }
    }

    let mut call_labels = HashSet::new();
    for (i, call) in def.verify.calls.iter().enumerate() {
        if !call_labels.insert(call.label(i)) {
            issues.push(ValidationIssue {
                origin: origin.into(),
                message: format!("verify.calls[{i}]: duplicate call name `{}`", call.label(i)),
            });
        }
        // Anything else silently weakens the bound (e.g. `{min: 1}` becomes "0 or more").
        if call.count.as_ref().is_some_and(|c| !valid_count(c)) {
            issues.push(ValidationIssue {
                origin: origin.into(),
                message: format!(
                    "verify.calls[{i}]: `count` must be a non-negative integer or a map of eq/gte/gt/lte/lt integers whose lower bound does not exceed the upper bound"
                ),
            });
        }
    }

    // A stub that cannot answer is skipped at serve time and surfaces as a
    // misleading "no stub matched".
    let mut mounts = HashSet::new();
    for (name, dep) in &def.mocks {
        if !mounts.insert(dep.mount(name)) {
            issues.push(ValidationIssue {
                origin: origin.into(),
                message: format!(
                    "mocks.{name}: mount path `{}` is used by another dependency",
                    dep.mount(name)
                ),
            });
        }
        for (i, stub) in dep.stubs.iter().enumerate() {
            let at = format!("mocks.{name}.stubs[{i}]");
            let answers = stub.response.iter().chain(stub.responses.iter().flatten());
            if stub.response.is_some() == stub.responses.is_some()
                || stub.responses.as_ref().is_some_and(Vec::is_empty)
            {
                issues.push(ValidationIssue {
                    origin: origin.into(),
                    message: format!("{at}: needs exactly one non-empty `response` or `responses`"),
                });
            }
            if answers
                .clone()
                .any(|r| r.json.is_some() && r.body.is_some())
            {
                issues.push(ValidationIssue {
                    origin: origin.into(),
                    message: format!("{at}: a response sets both `json` and `body`"),
                });
            }
            if stub
                .match_
                .path
                .as_ref()
                .and_then(Json::as_str)
                .is_some_and(|p| p.contains('?'))
            {
                issues.push(ValidationIssue {
                    origin: origin.into(),
                    message: format!(
                        "{at}: match.path must not contain a query string; use match.query"
                    ),
                });
            }
        }
    }

    // ordered: groups must reference call expectation labels.
    let labels: HashSet<String> = def
        .verify
        .calls
        .iter()
        .enumerate()
        .map(|(i, c)| c.label(i))
        .collect();
    for (gi, group) in def.verify.ordered.iter().enumerate() {
        for name in group {
            if !labels.contains(name) {
                issues.push(ValidationIssue {
                    origin: origin.into(),
                    message: format!(
                        "verify.ordered[{gi}]: `{name}` does not name a calls: expectation"
                    ),
                });
            }
        }
    }
}

fn valid_count(count: &Json) -> bool {
    let bound = |v: &Json| v.is_u64() || v.as_str().is_some_and(|t| t.contains("{{"));
    match count {
        Json::Object(m) => {
            let shape_ok = !m.is_empty()
                && m.iter().all(|(k, v)| {
                    ["eq", "gte", "gt", "lte", "lt"].contains(&k.as_str()) && bound(v)
                });
            // Mirrors count_bounds in the mock crate: a lower bound above the upper bound can
            // never be satisfied and used to overflow the slot arithmetic.
            let get = |k: &str| m.get(k).and_then(Json::as_u64);
            let lo = [
                get("eq"),
                get("gte"),
                get("gt").map(|v| v.saturating_add(1)),
            ]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(0);
            let hi = [
                get("eq"),
                get("lte"),
                get("lt").map(|v| v.saturating_sub(1)),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(u64::MAX);
            shape_ok && lo <= hi
        }
        other => bound(other),
    }
}

/// Extract `X` from every `flow.X` inside `{{ }}` expressions.
fn find_flow_refs(s: &str) -> Vec<String> {
    find_refs(s, "flow.")
}

/// Extract `X` from every `steps.X` inside `{{ }}` expressions.
fn find_step_refs(s: &str) -> Vec<String> {
    find_refs(s, "steps.")
}

fn find_refs(s: &str, ns: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else { break };
        let expr = &after[..close];
        for (at, _) in expr.match_indices(ns) {
            let standalone = !expr[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
            let name: String = expr[at + ns.len()..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if standalone && !name.is_empty() {
                out.push(name);
            }
        }
        rest = &after[close + 2..];
    }
    out
}
