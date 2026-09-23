use serde_json::{json, Value};
use vault_dsl::{CallExpect, UnmatchedPolicy};
use vault_store::matchers::{self, MatchCtx};
use vault_store::{CheckFailure, CheckResult, FailureKind, NearMiss, VerifyOutcome};

use crate::matcher::{body_matches, component_score, full_score, request_matches};
use crate::record::RecordedExchange;
use crate::session::SessionReport;

pub fn verify_calls(
    report: &SessionReport,
    calls: &[CallExpect],
    ordered: &[Vec<String>],
    unexpected: &UnmatchedPolicy,
    ctx: &MatchCtx,
) -> VerifyOutcome {
    let mut out = VerifyOutcome::default();
    let recs = &report.recordings;

    // Serve-time 599s always fail: `unmatched: fail` was the dependency's contract.
    for hit in &report.unmatched_hits {
        out.checks.push(CheckResult::Fail(CheckFailure::new(
            format!(
                "mock `{}` received a request no stub matched (served {}): {} {}",
                hit.dependency,
                crate::UNMATCHED_STATUS,
                hit.request.method,
                hit.request.path
            ),
            format!("mocks.{}", hit.dependency),
            json!("every request to a mocked dependency must match a stub"),
            FailureKind::UnexpectedCall {
                exchange: hit.to_report_value(),
            },
        )));
    }

    let eligible: Vec<Vec<usize>> = calls
        .iter()
        .map(|c| {
            recs.iter()
                .enumerate()
                .filter(|(_, r)| call_matches(c, r, ctx))
                .map(|(j, _)| j)
                .collect()
        })
        .collect();

    let bounds: Vec<(u64, u64)> = calls.iter().map(|c| count_bounds(&c.count)).collect();

    // Lower bounds via bipartite matching: greedy assignment gives false
    // negatives when expectations overlap on the same recordings.
    let mut slot_owner: Vec<usize> = Vec::new();
    let mut slot_adj: Vec<Vec<usize>> = Vec::new();
    for (i, (lo, _)) in bounds.iter().enumerate() {
        for _ in 0..*lo {
            slot_owner.push(i);
            slot_adj.push(eligible[i].clone());
        }
    }
    let slot_match = matchers::max_bipartite(recs.len(), &slot_adj);

    let mut claimed: Vec<Option<usize>> = vec![None; recs.len()];
    let mut satisfied = vec![0u64; calls.len()];
    for (slot, rec) in slot_match.iter().enumerate() {
        if let Some(j) = rec {
            claimed[*j] = Some(slot_owner[slot]);
            satisfied[slot_owner[slot]] += 1;
        }
    }
    for (j, rec_claim) in claimed.iter_mut().enumerate() {
        if rec_claim.is_some() {
            continue;
        }
        for (i, elig) in eligible.iter().enumerate() {
            if elig.contains(&j) && satisfied[i] < bounds[i].1 {
                *rec_claim = Some(i);
                satisfied[i] += 1;
                break;
            }
        }
    }

    // A bounded expectation absorbs its own overflow so the same recording
    // isn't double-reported as both CountMismatch and UnexpectedCall.
    let mut overflow = vec![0u64; calls.len()];
    for (i, elig) in eligible.iter().enumerate() {
        if bounds[i].1 == u64::MAX {
            continue;
        }
        for &j in elig {
            if claimed[j].is_none() {
                claimed[j] = Some(i);
                overflow[i] += 1;
            }
        }
    }

    for (i, call) in calls.iter().enumerate() {
        let label = call.label(i);
        let (lo, hi) = bounds[i];
        let actual = satisfied[i];
        if actual < lo {
            let mut failure = CheckFailure::new(
                format!("expected call missing: {label} (satisfied {actual} of {lo})"),
                format!("verify.calls[{i}]"),
                expected_value(call),
                FailureKind::MissedCall { satisfied: actual },
            );
            failure.near_misses = near_misses(call, recs, ctx);
            out.checks.push(CheckResult::Fail(failure));
        } else if overflow[i] > 0 {
            out.checks.push(CheckResult::Fail(CheckFailure::new(
                format!("too many matching calls for {label}"),
                format!("verify.calls[{i}]"),
                expected_value(call),
                FailureKind::CountMismatch {
                    expected: count_desc(lo, hi),
                    actual: actual + overflow[i],
                },
            )));
        } else {
            out.push_pass(format!("calls: {label} ×{actual}"));
        }
    }

    if *unexpected == UnmatchedPolicy::Fail {
        for (j, rec) in recs.iter().enumerate() {
            if claimed[j].is_none()
                && rec.matched_stub.is_some()
                && report.mocked_deps.contains(&rec.dependency)
            {
                out.checks.push(CheckResult::Fail(CheckFailure::new(
                    format!(
                        "unexpected call to mocked dependency `{}`: {} {}",
                        rec.dependency, rec.request.method, rec.request.path
                    ),
                    "verify.unexpected",
                    json!("no calls: expectation claims this recording"),
                    FailureKind::UnexpectedCall {
                        exchange: rec.to_report_value(),
                    },
                )));
            }
        }
    }

    for (gi, group) in ordered.iter().enumerate() {
        check_order(gi, group, calls, recs, &claimed, &mut out);
    }

    out
}

fn call_matches(call: &CallExpect, rec: &RecordedExchange, ctx: &MatchCtx) -> bool {
    rec.dependency == call.dependency
        && request_matches(&call.match_, &rec.request)
        && call
            .body
            .as_ref()
            .map(|b| body_matches(b, &rec.request, ctx))
            .unwrap_or(true)
}

fn count_bounds(count: &Option<Value>) -> (u64, u64) {
    match count {
        None => (1, u64::MAX),
        Some(Value::Number(n)) => {
            let v = n.as_u64().unwrap_or(0);
            (v, v)
        }
        Some(Value::Object(m)) => {
            let mut lo = 0;
            let mut hi = u64::MAX;
            if let Some(v) = m.get("eq").and_then(Value::as_u64) {
                lo = v;
                hi = v;
            }
            if let Some(v) = m.get("gte").and_then(Value::as_u64) {
                lo = lo.max(v);
            }
            if let Some(v) = m.get("gt").and_then(Value::as_u64) {
                lo = lo.max(v + 1);
            }
            if let Some(v) = m.get("lte").and_then(Value::as_u64) {
                hi = hi.min(v);
            }
            if let Some(v) = m.get("lt").and_then(Value::as_u64) {
                hi = hi.min(v.saturating_sub(1));
            }
            (lo, hi)
        }
        _ => (1, u64::MAX),
    }
}

fn count_desc(lo: u64, hi: u64) -> String {
    if lo == hi {
        format!("exactly {lo}")
    } else if hi == u64::MAX {
        format!(">= {lo}")
    } else {
        format!("{lo}..={hi}")
    }
}

fn expected_value(call: &CallExpect) -> Value {
    json!({
        "dependency": call.dependency,
        "match": serde_json::to_value(&call.match_).unwrap_or(Value::Null),
        "body": call.body,
        "count": call.count,
    })
}

fn near_misses(call: &CallExpect, recs: &[RecordedExchange], ctx: &MatchCtx) -> Vec<NearMiss> {
    let full = full_score(&call.match_) + call.body.as_ref().map(|_| 0).unwrap_or(0) + 3;
    let mut scored: Vec<NearMiss> = recs
        .iter()
        .filter(|r| r.dependency == call.dependency)
        .map(|r| {
            let score = (component_score(&call.match_, &r.request) + 3) as f32 / full as f32;
            let diffs = call
                .body
                .as_ref()
                .and_then(|b| b.get("json_partial"))
                .map(|partial| matchers::json_contains(partial, &r.request.body_json, ctx))
                .unwrap_or_default();
            NearMiss {
                actual: r.to_report_value(),
                diffs,
                score,
            }
        })
        .filter(|nm| nm.score >= 0.3)
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(3);
    scored
}

/// Subsequence semantics: each group must admit a strictly-increasing pick of
/// one claimed seq per label.
fn check_order(
    gi: usize,
    group: &[String],
    calls: &[CallExpect],
    recs: &[RecordedExchange],
    claimed: &[Option<usize>],
    out: &mut VerifyOutcome,
) {
    let mut prev: i64 = -1;
    for name in group {
        let Some(idx) = calls
            .iter()
            .enumerate()
            .position(|(i, c)| &c.label(i) == name)
        else {
            continue;
        };
        let mut seqs: Vec<u64> = claimed
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == Some(idx))
            .map(|(j, _)| recs[j].seq)
            .filter(|s| (*s as i64) > prev)
            .collect();
        seqs.sort_unstable();
        match seqs.first() {
            Some(s) => prev = *s as i64,
            None => {
                let interleaving: Vec<(String, u64)> = claimed
                    .iter()
                    .enumerate()
                    .filter_map(|(j, c)| c.map(|i| (calls[i].label(i), recs[j].seq)))
                    .collect();
                out.checks.push(CheckResult::Fail(CheckFailure::new(
                    format!("order violated in group {gi}: `{name}` did not occur after its predecessor"),
                    format!("verify.ordered[{gi}]"),
                    json!(group),
                    FailureKind::OrderViolation { interleaving },
                )));
                return;
            }
        }
    }
    out.push_pass(format!("ordered[{gi}]: {}", group.join(" → ")));
}
