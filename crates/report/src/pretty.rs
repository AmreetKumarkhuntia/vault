use comfy_table::{presets, Cell, Table};
use owo_colors::OwoColorize;
use vault_core::{RunResult, TestResult, TestStatus};
use vault_store::{CheckFailure, FailureKind};

pub fn print_run(run: &RunResult, verbose: bool) {
    for test in &run.tests {
        print_test(test, verbose);
    }
    println!();
    let line = format!(
        "{} passed · {} failed · {} errored · {} skipped · {}ms",
        run.count(TestStatus::Passed),
        run.count(TestStatus::Failed),
        run.count(TestStatus::Errored),
        run.count(TestStatus::Skipped),
        run.duration_ms
    );
    match run.status() {
        TestStatus::Passed | TestStatus::Skipped => println!("{}", line.green().bold()),
        TestStatus::Failed => println!("{}", line.red().bold()),
        TestStatus::Errored => println!("{}", line.yellow().bold()),
    }
}

fn print_test(test: &TestResult, verbose: bool) {
    let flow_prefix = test
        .flow
        .as_deref()
        .map(|f| format!("[{f}] "))
        .unwrap_or_default();
    match test.status {
        TestStatus::Passed => {
            println!(
                "{} {}{} ({}ms)",
                "✓".green().bold(),
                flow_prefix,
                test.name,
                test.duration_ms
            );
            if verbose {
                for step in &test.steps {
                    println!(
                        "    {} step {} ({} attempts)",
                        "✓".green(),
                        step.name,
                        step.attempts
                    );
                }
            }
        }
        TestStatus::Skipped => {
            println!(
                "{} {}{} — {}",
                "○".dimmed(),
                flow_prefix,
                test.name.dimmed(),
                test.skip_reason.as_deref().unwrap_or("skipped").dimmed()
            );
        }
        TestStatus::Errored => {
            println!(
                "{} {}{} ({}ms)",
                "⚠".yellow().bold(),
                flow_prefix,
                test.name,
                test.duration_ms
            );
            if let Some(e) = &test.error {
                println!("    {}", e.yellow());
            }
        }
        TestStatus::Failed => {
            println!(
                "{} {}{} ({}ms)",
                "✗".red().bold(),
                flow_prefix,
                test.name,
                test.duration_ms
            );
            for step in &test.steps {
                let ok = step.status == TestStatus::Passed;
                let glyph = if ok {
                    "✓".green().to_string()
                } else {
                    "✗".red().to_string()
                };
                println!("    {glyph} step {}", step.name);
                for f in step.checks.failures() {
                    print_failure(f, 6);
                }
            }
            for f in test.verify.failures() {
                print_failure(f, 4);
            }
        }
    }
}

fn print_failure(f: &CheckFailure, indent: usize) {
    let pad = " ".repeat(indent);
    let attempts = if f.attempts > 1 {
        format!(" (after {}ms, {} attempts)", f.elapsed_ms, f.attempts)
    } else {
        String::new()
    };
    println!(
        "{pad}{} {}{}",
        "✗".red(),
        f.description.red(),
        attempts.dimmed()
    );
    println!("{pad}  {} {}", "at".dimmed(), f.yaml_path.dimmed());

    match &f.kind {
        FailureKind::ValueMismatch { diffs } if !diffs.is_empty() => {
            let mut table = diff_table();
            for d in diffs.iter().take(10) {
                table.add_row(vec![
                    Cell::new(&d.path),
                    Cell::new(compact(&d.expected)),
                    Cell::new(compact(&d.actual)),
                ]);
            }
            indent_print(&table, indent + 2);
        }
        FailureKind::MissedCall { .. } if !f.near_misses.is_empty() => {
            let best = &f.near_misses[0];
            let method = best
                .actual
                .pointer("/request/method")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let path = best
                .actual
                .pointer("/request/path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let body = best
                .actual
                .pointer("/request/body")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            println!("{pad}  closest recorded call (score {:.2}):", best.score);
            println!(
                "{pad}    {} {} {}",
                method,
                path,
                truncate(body, 200).dimmed()
            );
            if !best.diffs.is_empty() {
                let mut table = diff_table();
                for d in best.diffs.iter().take(10) {
                    table.add_row(vec![
                        Cell::new(&d.path),
                        Cell::new(compact(&d.expected)),
                        Cell::new(compact(&d.actual)),
                    ]);
                }
                indent_print(&table, indent + 2);
            }
        }
        FailureKind::MissingRow if !f.near_misses.is_empty() => {
            let best = &f.near_misses[0];
            println!("{pad}  closest match (score {:.2}):", best.score);
            let mut table = diff_table();
            if let Some(exp) = f.expected.as_object() {
                for (col, want) in exp {
                    let got = best
                        .actual
                        .get(col)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let matched = !best.diffs.iter().any(|d| d.path == *col);
                    let mark = if matched { "✓" } else { "✗" };
                    table.add_row(vec![
                        Cell::new(col),
                        Cell::new(describe(want)),
                        Cell::new(format!("{} {}", compact(&got), mark)),
                    ]);
                }
            }
            indent_print(&table, indent + 2);
        }
        FailureKind::UnexpectedCall { exchange } => {
            let method = exchange
                .pointer("/request/method")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let path = exchange
                .pointer("/request/path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let body = exchange
                .pointer("/request/body")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            println!(
                "{pad}  {} {} {}",
                method,
                path,
                truncate(body, 200).dimmed()
            );
        }
        FailureKind::UnexpectedRow { actual }
        | FailureKind::UnexpectedChange { after: actual, .. } => {
            println!("{pad}  row: {}", truncate(&actual.to_string(), 300));
        }
        FailureKind::CountMismatch { expected, actual } => {
            println!("{pad}  expected {expected}, got {actual}");
        }
        FailureKind::OrderViolation { interleaving } => {
            for (name, seq) in interleaving {
                println!("{pad}  seq {seq}: {name}");
            }
        }
        _ => {}
    }
}

fn diff_table() -> Table {
    let mut table = Table::new();
    table.load_style(presets::UTF8_BORDERS_ONLY);
    table.set_header(vec!["field", "expected", "actual"]);
    table
}

fn indent_print(table: &Table, indent: usize) {
    let pad = " ".repeat(indent);
    for line in table.to_string().lines() {
        println!("{pad}{line}");
    }
}

fn compact(v: &serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    truncate(&s, 60)
}

fn describe(v: &serde_json::Value) -> String {
    truncate(&vault_store::matchers::describe(v), 60)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}
