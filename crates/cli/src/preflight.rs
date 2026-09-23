use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use owo_colors::OwoColorize;
use vault_dsl::Environment;
use vault_mock::MockServer;
use vault_store::StateStore;

/// Probe everything before test 1: never let forty tests each time out
/// against a target that isn't running.
pub async fn check(
    env: &Environment,
    stores: &IndexMap<String, Arc<dyn StateStore>>,
    mock: &Arc<MockServer>,
) -> Result<(), i32> {
    let mut rows: Vec<(String, Result<String, String>)> = Vec::new();

    let health = env.target.health_check.clone();
    let target_result = match &health {
        Some(h) => {
            let url = format!(
                "{}/{}",
                env.target.base_url.trim_end_matches('/'),
                h.path.trim_start_matches('/')
            );
            let client = reqwest::Client::new();
            let deadline = Instant::now() + h.timeout;
            loop {
                match client
                    .get(&url)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => break Ok(format!("{} OK", r.status())),
                    Ok(r) => {
                        if Instant::now() >= deadline {
                            break Err(format!("health returned {}", r.status()));
                        }
                    }
                    Err(e) => {
                        if Instant::now() >= deadline {
                            break Err(e.to_string());
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        None => Ok("no health_check configured — skipped".into()),
    };
    rows.push((format!("target {}", env.target.base_url), target_result));

    for (kind, store) in stores {
        let r = store
            .ping()
            .await
            .map(|_| "OK".to_string())
            .map_err(|e| e.to_string());
        rows.push((kind.to_string(), r));
    }
    rows.push((
        format!("mock listener {}", mock.base_url()),
        Ok("bound".into()),
    ));

    let failed = rows.iter().any(|(_, r)| r.is_err());
    if failed {
        eprintln!("{}", "preflight failed:".red().bold());
    }
    for (name, r) in &rows {
        match r {
            Ok(msg) => eprintln!("  {} {name} — {msg}", "✓".green()),
            Err(msg) => eprintln!("  {} {name} — {msg}", "✗".red()),
        }
    }
    if failed {
        Err(3)
    } else {
        Ok(())
    }
}
