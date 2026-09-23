use vault_core::{RunResult, TestStatus};

pub fn to_junit_xml(run: &RunResult) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuite name=\"vault\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">\n",
        run.tests.len(),
        run.count(TestStatus::Failed),
        run.count(TestStatus::Errored),
        run.count(TestStatus::Skipped),
        run.duration_ms as f64 / 1000.0
    ));
    for t in &run.tests {
        out.push_str(&format!(
            "  <testcase name=\"{}\" time=\"{:.3}\"",
            escape(&t.name),
            t.duration_ms as f64 / 1000.0
        ));
        match t.status {
            TestStatus::Passed => out.push_str("/>\n"),
            TestStatus::Skipped => {
                out.push_str(&format!(
                    ">\n    <skipped message=\"{}\"/>\n  </testcase>\n",
                    escape(t.skip_reason.as_deref().unwrap_or("skipped"))
                ));
            }
            TestStatus::Errored => {
                out.push_str(&format!(
                    ">\n    <error message=\"{}\"/>\n  </testcase>\n",
                    escape(t.error.as_deref().unwrap_or("errored"))
                ));
            }
            TestStatus::Failed => {
                let msgs: Vec<String> = t
                    .steps
                    .iter()
                    .flat_map(|s| s.checks.failures())
                    .chain(t.verify.failures())
                    .map(|f| f.description.clone())
                    .collect();
                out.push_str(&format!(
                    ">\n    <failure message=\"{}\"/>\n  </testcase>\n",
                    escape(&msgs.join("; "))
                ));
            }
        }
    }
    out.push_str("</testsuite>\n");
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
