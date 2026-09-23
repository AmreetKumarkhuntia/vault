use std::io::Write;
use std::sync::Arc;

use indexmap::IndexMap;
use owo_colors::OwoColorize;
use vault_core::{Gate, GateDecision, PausePoint, PauseSnapshot};
use vault_store::StateStore;

pub struct InteractiveGate {
    stores: IndexMap<String, Arc<dyn StateStore>>,
}

impl InteractiveGate {
    pub fn new(stores: IndexMap<String, Arc<dyn StateStore>>) -> Self {
        Self { stores }
    }
}

impl Gate for InteractiveGate {
    fn pause(&self, point: &PausePoint, snap: &PauseSnapshot) -> GateDecision {
        let label = match point {
            PausePoint::AfterSeed => "seeded".to_string(),
            PausePoint::MocksArmed => "mocks armed".to_string(),
            PausePoint::AfterStep(name) => format!("after step `{name}`"),
            PausePoint::BeforeVerify => "before verify".to_string(),
        };
        println!(
            "\n{} {} — {} {}",
            "⏸".cyan().bold(),
            snap.test.bold(),
            label,
            "(c=continue, resp, vars, calls, db <sql>, redis <cmd>, q=abort test, Q=abort run)"
                .dimmed()
        );
        loop {
            print!("{} ", "step>".cyan());
            std::io::stdout().flush().ok();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return GateDecision::Continue;
            }
            let line = line.trim();
            match line {
                "" | "c" | "n" => return GateDecision::Continue,
                "q" => return GateDecision::AbortTest,
                "Q" => return GateDecision::AbortRun,
                "resp" => match snap.last_response {
                    Some(r) => {
                        println!("status: {} ({}ms)", r.status, r.elapsed.as_millis());
                        println!(
                            "headers: {}",
                            serde_json::to_string_pretty(&r.headers).unwrap_or_default()
                        );
                        println!(
                            "body: {}",
                            if r.body_json.is_null() {
                                r.body.clone()
                            } else {
                                serde_json::to_string_pretty(&r.body_json).unwrap_or_default()
                            }
                        );
                    }
                    None => println!("no response yet"),
                },
                "vars" => {
                    let pruned = serde_json::json!({
                        "vars": snap.ctx.get("vars"),
                        "steps": snap.ctx.get("steps"),
                        "flow": snap.ctx.get("flow"),
                        "mock": snap.ctx.get("mock"),
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&pruned).unwrap_or_default()
                    );
                }
                "calls" => {
                    if snap.recordings.is_empty() {
                        println!("no recorded calls yet");
                    }
                    for r in &snap.recordings {
                        println!(
                            "  #{} {} {} {} → {} {}",
                            r.seq,
                            r.dependency,
                            r.request.method,
                            r.request.path,
                            r.responded_status,
                            r.matched_stub.as_deref().unwrap_or("UNMATCHED").dimmed()
                        );
                    }
                }
                other if other.starts_with("db ") => {
                    self.inspect("postgres", other.trim_start_matches("db "));
                }
                other if other.starts_with("redis ") => {
                    self.inspect("redis", other.trim_start_matches("redis "));
                }
                other => println!("unknown command `{other}`"),
            }
        }
    }
}

impl InteractiveGate {
    fn inspect(&self, kind: &str, query: &str) {
        let Some(store) = self.stores.get(kind).cloned() else {
            println!("store `{kind}` not configured");
            return;
        };
        let query = query.to_string();
        let handle = tokio::runtime::Handle::current();
        // Gate runs synchronously inside the runtime; block_on directly would
        // panic, so the async inspect runs on a helper thread.
        let result = std::thread::spawn(move || handle.block_on(store.inspect(&query)))
            .join()
            .unwrap_or_else(|_| Err(vault_store::StoreError::Harness("inspect panicked".into())));
        match result {
            Ok(table) => {
                println!("{}", table.columns.join(" | "));
                for row in table.rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    println!("{}", cells.join(" | "));
                }
            }
            Err(e) => println!("{e}"),
        }
    }
}
