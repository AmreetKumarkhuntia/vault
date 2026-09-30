use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anstream::{eprintln, println};
use indexmap::IndexMap;
use owo_colors::OwoColorize;
use serde_json::{json, Value};
use vault_core::{NoopGate, RunResult, TestRunner};
use vault_dsl::Suite;
use vault_mock::MockServer;
use vault_store::{
    DocMode, PreparedStoreDoc, StoreConnConfig, StoreDocContext, StoreDocScope, StoreRegistry,
};

use crate::gate::InteractiveGate;
use crate::preflight;

pub struct RunArgs {
    pub pattern: Option<String>,
    pub tags: Vec<String>,
    pub env: String,
    pub suite_dir: String,
    pub step: bool,
    pub shuffle: bool,
    pub seed: Option<u64>,
    pub report: Option<String>,
    pub junit: Option<String>,
    pub verbose: bool,
}

pub fn registry() -> StoreRegistry {
    let mut reg = StoreRegistry::new();
    reg.register(Arc::new(vault_store_postgres::PostgresDriver));
    reg.register(Arc::new(vault_store_redis::RedisDriver));
    reg
}

fn resolve_suite_root(suite_path: &str) -> Result<PathBuf, i32> {
    let path = Path::new(suite_path);
    if path.is_dir() {
        return Ok(path.to_path_buf());
    }
    if path.is_file() && path.file_name().is_some_and(|name| name == "vault.yaml") {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        return Ok(if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        });
    }

    let detail = if path.exists() {
        "expected a suite directory or its vault.yaml file"
    } else {
        "path does not exist"
    };
    eprintln!(
        "{} `{}`: {detail}",
        "config error:".red().bold(),
        path.display()
    );
    Err(2)
}

fn load_suite(suite_path: &str) -> Result<Suite, i32> {
    let root = resolve_suite_root(suite_path)?;
    match vault_dsl::discover(&root) {
        Ok(s) => Ok(s),
        Err(e) => {
            eprintln!("{} {e}", "config error:".red().bold());
            Err(2)
        }
    }
}

fn validate_environment_stores(
    suite: &Suite,
    environment: &vault_dsl::Environment,
    active_tests: &HashSet<PathBuf>,
) -> Vec<String> {
    let configured: HashSet<&str> = environment.stores.keys().map(String::as_str).collect();
    let mut issues = Vec::new();

    if !active_tests.is_empty() {
        for kind in suite.config.seed.keys() {
            if !configured.contains(kind.as_str()) {
                issues.push(format!(
                    "{}: global seed uses store `{kind}` but it is not configured in the selected environment",
                    suite.root.join("vault.yaml").display()
                ));
            }
        }
    }

    for test in suite
        .tests
        .iter()
        .filter(|test| active_tests.contains(&test.path))
    {
        let origin = test.path.display();
        for kind in test.def.seed.keys().chain(test.def.verify.stores.keys()) {
            if !configured.contains(kind.as_str()) {
                issues.push(format!(
                    "{origin}: store `{kind}` is used but not configured in the selected environment"
                ));
            }
        }
        if !test.def.watch.is_empty() && !configured.contains("postgres") {
            issues.push(format!(
                "{origin}: `watch` requires a configured `postgres` store"
            ));
        }
    }

    issues
}

#[derive(Default)]
struct PreparedFixtures {
    global: IndexMap<String, PreparedStoreDoc>,
    local: HashMap<PathBuf, IndexMap<String, PreparedStoreDoc>>,
}

fn prepare_all(
    suite: &mut Suite,
    reg: &StoreRegistry,
    selection: Option<&FixtureSelection>,
) -> (PreparedFixtures, Vec<String>) {
    let mut issues = Vec::new();
    let mut prepared = PreparedFixtures::default();
    let global_origin = suite.root.join("vault.yaml");
    let global_context = StoreDocContext::new(
        suite.root.clone(),
        global_origin.clone(),
        StoreDocScope::Global,
    );
    let prepare_global = selection.is_none_or(|selection| !selection.active.is_empty());
    if prepare_global {
        let mut global_docs = IndexMap::new();
        for (kind, doc) in &suite.config.seed {
            match reg.get(kind) {
                Some(driver) => {
                    match driver.prepare_with_context(doc, DocMode::Seed, &global_context) {
                        Ok(doc) => {
                            global_docs.insert(kind.clone(), doc.doc().clone());
                            prepared.global.insert(kind.clone(), doc);
                        }
                        Err(error) => issues.push(format!("{}: {error}", global_origin.display())),
                    }
                }
                None => issues.push(format!(
                    "{}: unknown store `{kind}` in global seed:",
                    global_origin.display()
                )),
            }
        }
        suite.config.seed = global_docs;
    }

    for t in &mut suite.tests {
        if selection.is_some_and(|selection| !selection.active.contains(&t.path)) {
            continue;
        }
        let origin = t.path.display().to_string();
        let context =
            StoreDocContext::new(suite.root.clone(), t.path.clone(), StoreDocScope::Local);
        let mut local_docs = IndexMap::new();
        let mut local_prepared = IndexMap::new();
        for (kind, doc) in &t.def.seed {
            match reg.get(kind) {
                Some(driver) => match driver.prepare_with_context(doc, DocMode::Seed, &context) {
                    Ok(doc) => {
                        local_docs.insert(kind.clone(), doc.doc().clone());
                        local_prepared.insert(kind.clone(), doc);
                    }
                    Err(error) => issues.push(format!("{origin}: {error}")),
                },
                None => issues.push(format!("{origin}: unknown store `{kind}` in seed:")),
            }
        }
        t.def.seed = local_docs;
        for (kind, doc) in &t.def.verify.stores {
            match reg.get(kind) {
                Some(driver) => {
                    if let Err(e) = driver.validate_with_context(doc, DocMode::Verify, &context) {
                        issues.push(format!("{origin}: {e}"));
                    }
                }
                None => issues.push(format!("{origin}: unknown store `{kind}` in verify:")),
            }
        }

        if selection.is_none_or(|selection| selection.with_global.contains(&t.path)) {
            for (kind, local) in &local_prepared {
                if let Some(global) = prepared.global.get(kind) {
                    let mut effective = global.clone();
                    if let Err(error) = effective.append(local.clone()) {
                        issues.push(format!("{origin}: {error}"));
                    }
                }
            }
        }
        prepared.local.insert(t.path.clone(), local_prepared);
    }

    (prepared, issues)
}

#[derive(Default)]
struct FixtureSelection {
    active: HashSet<PathBuf>,
    with_global: HashSet<PathBuf>,
}

fn fixture_selection(suite: &Suite, plan: &Plan<'_>) -> FixtureSelection {
    let tests_by_name: HashMap<&str, &vault_dsl::LoadedTest> = suite
        .tests
        .iter()
        .map(|test| (test.def.test.as_str(), test))
        .collect();
    let mut selection = FixtureSelection::default();
    for flow in &plan.flows {
        let mut reset_pending = flow.def.reset == vault_dsl::FlowReset::Once;
        for stage in &flow.def.stages {
            if let Some(test) = tests_by_name.get(stage.test.as_str()) {
                if test.def.skip.reason().is_none() {
                    selection.active.insert(test.path.clone());
                    if flow.def.reset == vault_dsl::FlowReset::Each
                        || reset_pending
                        || flow.def.on_failure == vault_dsl::FlowOnFailure::Continue
                    {
                        selection.with_global.insert(test.path.clone());
                    }
                    if reset_pending {
                        reset_pending = false;
                    }
                }
            }
        }
    }
    for test in &plan.standalone {
        if test.def.skip.reason().is_none() {
            selection.active.insert(test.path.clone());
            selection.with_global.insert(test.path.clone());
        }
    }
    selection
}

struct Plan<'s> {
    standalone: Vec<&'s vault_dsl::LoadedTest>,
    flows: Vec<&'s vault_dsl::LoadedFlow>,
}

impl Plan<'_> {
    fn is_empty(&self) -> bool {
        self.standalone.is_empty() && self.flows.is_empty()
    }
}

fn plan<'s>(suite: &'s Suite, pattern: &Option<String>, tags: &[String]) -> Result<Plan<'s>, i32> {
    let matcher = match pattern {
        Some(p) => {
            let glob = globset::GlobBuilder::new(p)
                .literal_separator(false)
                .build()
                .map_err(|e| {
                    eprintln!("{} bad pattern: {e}", "usage error:".red().bold());
                    2
                })?
                .compile_matcher();
            Some(glob)
        }
        None => None,
    };
    let name_matches = |name: &str| {
        matcher
            .as_ref()
            .map(|m| m.is_match(name) || name == pattern.as_deref().unwrap_or(""))
            .unwrap_or(true)
    };
    let tags_match = |t: &[String]| tags.iter().all(|want| t.contains(want));

    let in_flows: HashSet<&str> = suite
        .flows
        .iter()
        .flat_map(|f| f.def.stages.iter().map(|s| s.test.as_str()))
        .collect();

    let flows: Vec<_> = suite
        .flows
        .iter()
        .filter(|f| name_matches(&f.def.flow) && tags_match(&f.def.tags))
        .collect();

    let standalone: Vec<_> = suite
        .tests
        .iter()
        .filter(|t| tags_match(&t.def.tags))
        .filter(|t| match &matcher {
            // Flow-member tests run inside their flow unless named explicitly.
            None => !in_flows.contains(t.def.test.as_str()),
            Some(m) => m.is_match(&t.def.test),
        })
        .collect();

    Ok(Plan { standalone, flows })
}

fn print_empty_selection(pattern: &Option<String>, tags: &[String]) {
    let pattern = pattern.as_deref().unwrap_or("<none>");
    let tags = if tags.is_empty() {
        "<none>".to_string()
    } else {
        tags.join(", ")
    };
    eprintln!(
        "{} no tests or flows matched the requested selection",
        "usage error:".red().bold()
    );
    eprintln!("  pattern: {pattern}");
    eprintln!("  tags: {tags}");
}

#[derive(Debug)]
struct ReportWriteFailure {
    format: &'static str,
    path: String,
    error: std::io::Error,
}

fn write_requested_reports(
    run: &RunResult,
    json_path: Option<String>,
    junit_path: Option<String>,
) -> Vec<ReportWriteFailure> {
    let mut failures = Vec::new();

    if let Some(path) = json_path {
        match vault_report::write_json(run, &path) {
            Ok(()) => println!("{} JSON report: {path}", "→".cyan()),
            Err(error) => failures.push(ReportWriteFailure {
                format: "JSON",
                path,
                error,
            }),
        }
    }

    if let Some(path) = junit_path {
        match vault_report::write_junit(run, &path) {
            Ok(()) => println!("{} JUnit report: {path}", "→".cyan()),
            Err(error) => failures.push(ReportWriteFailure {
                format: "JUnit",
                path,
                error,
            }),
        }
    }

    failures
}

fn report_aware_exit_code(run_exit_code: i32, report_write_failed: bool) -> i32 {
    if report_write_failed {
        3
    } else {
        run_exit_code
    }
}

pub fn list(pattern: Option<String>, tags: Vec<String>, suite_dir: String, fixtures: bool) -> i32 {
    let mut suite = match load_suite(&suite_dir) {
        Ok(s) => s,
        Err(c) => return c,
    };
    if fixtures {
        let issues: Vec<String> = vault_dsl::validate_suite(&suite)
            .into_iter()
            .map(|issue| issue.to_string())
            .collect();
        if !issues.is_empty() {
            for issue in &issues {
                eprintln!("{} {issue}", "✗".red());
            }
            eprintln!("\n{} {} issue(s)", "invalid:".red().bold(), issues.len());
            return 2;
        }
    }
    let mut selected_plan = match plan(&suite, &pattern, &tags) {
        Ok(p) => p,
        Err(c) => return c,
    };
    let mut prepared = PreparedFixtures::default();
    if fixtures {
        let selection = fixture_selection(&suite, &selected_plan);
        drop(selected_plan);
        let (resolved, issues) = prepare_all(&mut suite, &registry(), Some(&selection));
        if !issues.is_empty() {
            for issue in &issues {
                eprintln!("{} {issue}", "✗".red());
            }
            eprintln!("\n{} {} issue(s)", "invalid:".red().bold(), issues.len());
            return 2;
        }
        prepared = resolved;
        selected_plan = match plan(&suite, &pattern, &tags) {
            Ok(plan) => plan,
            Err(code) => return code,
        };
    }
    for f in &selected_plan.flows {
        println!("{} {}", "flow".cyan().bold(), f.def.flow);
        for (i, stage) in f.def.stages.iter().enumerate() {
            println!("    {}. {}", i + 1, stage.test);
        }
    }
    for t in &selected_plan.standalone {
        let tags = if t.def.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", t.def.tags.join(", "))
        };
        println!("{} {}{}", "test".green().bold(), t.def.test, tags.dimmed());
    }
    println!(
        "\n{} flows, {} standalone tests",
        selected_plan.flows.len(),
        selected_plan.standalone.len()
    );
    if fixtures {
        print_fixture_plan(&suite, &selected_plan, &prepared);
    }
    0
}

fn print_fixture_plan(suite: &Suite, plan: &Plan<'_>, prepared: &PreparedFixtures) {
    let tests_by_name: HashMap<&str, &vault_dsl::LoadedTest> = suite
        .tests
        .iter()
        .map(|test| (test.def.test.as_str(), test))
        .collect();

    println!("\n{}", "resolved SQL fixtures".cyan().bold());
    let mut printed_any = false;

    for flow in &plan.flows {
        println!(
            "  flow {} (reset: {})",
            flow.def.flow,
            match flow.def.reset {
                vault_dsl::FlowReset::Once => "once",
                vault_dsl::FlowReset::Each => "each",
            }
        );
        let mut first_reset_bound_stage = flow.def.reset == vault_dsl::FlowReset::Once;
        for (stage_index, stage) in flow.def.stages.iter().enumerate() {
            let Some(test) = tests_by_name.get(stage.test.as_str()) else {
                continue;
            };
            if test.def.skip.reason().is_some() {
                println!(
                    "    {}. {} [skipped; no fixture execution]",
                    stage_index + 1,
                    stage.test
                );
                continue;
            }
            let include_global = flow.def.reset == vault_dsl::FlowReset::Each
                || first_reset_bound_stage
                || flow.def.on_failure == vault_dsl::FlowOnFailure::Continue;
            let conditional_global = flow.def.reset == vault_dsl::FlowReset::Once
                && !first_reset_bound_stage
                && flow.def.on_failure == vault_dsl::FlowOnFailure::Continue;
            let label = if conditional_global {
                format!(
                    "    {}. {} [global fixtures run only if initialization is still pending]",
                    stage_index + 1,
                    stage.test
                )
            } else {
                format!("    {}. {}", stage_index + 1, stage.test)
            };
            printed_any |=
                print_test_fixture_plan(test, prepared, include_global, &label, "      ");
            if first_reset_bound_stage {
                first_reset_bound_stage = false;
            }
        }
    }

    for test in &plan.standalone {
        if test.def.skip.reason().is_some() {
            println!("  {} [skipped; no fixture execution]", test.def.test);
            continue;
        }
        printed_any |= print_test_fixture_plan(
            test,
            prepared,
            true,
            &format!("  {}", test.def.test),
            "    ",
        );
    }
    if !printed_any {
        println!("  <none>");
    }
}

fn print_test_fixture_plan(
    test: &vault_dsl::LoadedTest,
    prepared: &PreparedFixtures,
    include_global: bool,
    label: &str,
    indent: &str,
) -> bool {
    let local = prepared.local.get(&test.path);
    let mut kinds: Vec<&String> = local.into_iter().flat_map(|docs| docs.keys()).collect();
    if include_global {
        kinds.extend(prepared.global.keys());
    }
    kinds.sort();
    kinds.dedup();

    let mut lines = Vec::new();
    for kind in kinds {
        let effective = match (
            include_global.then(|| prepared.global.get(kind)).flatten(),
            local.and_then(|docs| docs.get(kind)),
        ) {
            (Some(global), Some(local)) => {
                let mut combined = global.clone();
                combined
                    .append(local.clone())
                    .expect("effective fixture plan was validated");
                combined
            }
            (Some(global), None) => global.clone(),
            (None, Some(local)) => local.clone(),
            (None, None) => continue,
        };
        for source in effective.files() {
            lines.push(format!(
                "{indent}{}. {} [{}] {} `{}` in `{}` ({})\n{indent}   match `{}` -> `{}`",
                source.prepared_index + 1,
                kind,
                source.scope.as_str(),
                source.selector_kind,
                source.selector,
                source.declaring_yaml.display(),
                source.yaml_path,
                source.logical_path.display(),
                source.resolved_path.display()
            ));
        }
    }

    if lines.is_empty() {
        return false;
    }
    println!("{label}");
    for line in lines {
        println!("{line}");
    }
    true
}

pub fn validate(suite_dir: String) -> i32 {
    let mut suite = match load_suite(&suite_dir) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut issues: Vec<String> = vault_dsl::validate_suite(&suite)
        .into_iter()
        .map(|issue| issue.to_string())
        .collect();
    let (_, store_issues) = prepare_all(&mut suite, &registry(), None);
    issues.extend(store_issues);
    if issues.is_empty() {
        println!(
            "{} {} tests, {} flows — no issues",
            "valid:".green().bold(),
            suite.tests.len(),
            suite.flows.len()
        );
        0
    } else {
        for i in &issues {
            eprintln!("{} {i}", "✗".red());
        }
        eprintln!("\n{} {} issue(s)", "invalid:".red().bold(), issues.len());
        2
    }
}

pub fn print_env(env: String, suite_dir: String) -> i32 {
    let suite = match load_suite(&suite_dir) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let Some(environment) = suite.config.environments.get(&env) else {
        eprintln!(
            "{} unknown environment `{env}`",
            "config error:".red().bold()
        );
        return 2;
    };
    let bind = &environment.mock_server.bind;
    if bind.ends_with(":0") {
        eprintln!(
            "{} mock_server.bind uses an ephemeral port ({bind}); give it a fixed port so the target can be started before the run",
            "warning:".yellow().bold()
        );
    }
    let mut deps: Vec<String> = suite
        .tests
        .iter()
        .flat_map(|t| t.def.mocks.keys().cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    deps.sort();
    for dep in deps {
        println!(
            "export {}_URL=http://{bind}/{dep}",
            dep.to_uppercase().replace('-', "_")
        );
    }
    for (kind, store) in &environment.stores {
        println!("export {}_URL={}", kind.to_uppercase(), store.url);
    }
    0
}

pub fn run(args: RunArgs) -> i32 {
    let mut suite = match load_suite(&args.suite_dir) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let reg = registry();
    let issues: Vec<String> = vault_dsl::validate_suite(&suite)
        .into_iter()
        .map(|issue| issue.to_string())
        .collect();
    if !issues.is_empty() {
        for i in &issues {
            eprintln!("{} {i}", "✗".red());
        }
        eprintln!(
            "\n{} suite invalid — nothing was executed",
            "config error:".red().bold()
        );
        return 2;
    }
    let initial_plan = match plan(&suite, &args.pattern, &args.tags) {
        Ok(plan) => plan,
        Err(code) => return code,
    };
    if initial_plan.is_empty() {
        print_empty_selection(&args.pattern, &args.tags);
        return 2;
    }
    let selection = fixture_selection(&suite, &initial_plan);
    drop(initial_plan);
    let (_, issues) = prepare_all(&mut suite, &reg, Some(&selection));
    if !issues.is_empty() {
        for issue in &issues {
            eprintln!("{} {issue}", "✗".red());
        }
        eprintln!(
            "\n{} suite invalid — nothing was executed",
            "config error:".red().bold()
        );
        return 2;
    }
    let Some(environment) = suite.config.environments.get(&args.env).cloned() else {
        eprintln!(
            "{} unknown environment `{}`",
            "config error:".red().bold(),
            args.env
        );
        return 2;
    };
    let store_issues = validate_environment_stores(&suite, &environment, &selection.active);
    if !store_issues.is_empty() {
        for issue in &store_issues {
            eprintln!("{} {issue}", "✗".red());
        }
        eprintln!(
            "\n{} selected environment is missing required stores",
            "config error:".red().bold()
        );
        return 2;
    }
    if args.step && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!(
            "{} --step needs an interactive terminal",
            "usage error:".red().bold()
        );
        return 2;
    }

    let plan = match plan(&suite, &args.pattern, &args.tags) {
        Ok(plan) => plan,
        Err(code) => return code,
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async { run_async(args, &suite, plan, reg, environment).await })
}

async fn run_async<'s>(
    args: RunArgs,
    suite: &'s Suite,
    plan: Plan<'s>,
    reg: StoreRegistry,
    environment: vault_dsl::Environment,
) -> i32 {
    let mut stores: IndexMap<String, Arc<dyn vault_store::StateStore>> = IndexMap::new();
    for (kind, cfg) in &environment.stores {
        let Some(driver) = reg.get(kind) else {
            eprintln!(
                "{} no driver for store `{kind}`",
                "config error:".red().bold()
            );
            return 2;
        };
        let mut options: serde_json::Map<String, Value> = cfg
            .options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        options.insert("suite_root".into(), json!(suite.root.display().to_string()));
        let conn = StoreConnConfig {
            alias: kind.clone(),
            url: cfg.url.clone(),
            options: Value::Object(options),
        };
        match driver.connect(&conn).await {
            Ok(store) => {
                stores.insert(kind.clone(), store);
            }
            Err(e) => {
                eprintln!("{} {e}", "environment error:".red().bold());
                return 3;
            }
        }
    }

    let mock = match MockServer::start(&environment.mock_server.bind).await {
        Ok(m) => Arc::new(m),
        Err(e) => {
            eprintln!("{} {e}", "environment error:".red().bold());
            return 3;
        }
    };

    if let Err(code) = preflight::check(&environment, &stores, &mock).await {
        return code;
    }

    let gate: Arc<dyn vault_core::Gate> = if args.step {
        Arc::new(InteractiveGate::new(stores.clone()))
    } else {
        Arc::new(NoopGate)
    };

    let runner = TestRunner {
        client: reqwest::Client::new(),
        target_base_url: environment.target.base_url.clone(),
        defaults: suite.config.defaults.clone(),
        global_seed: suite.config.seed.clone(),
        stores,
        mock,
        gate,
        abort_run: std::sync::atomic::AtomicBool::new(false),
    };

    let tests_by_name: HashMap<String, &vault_dsl::TestDef> = suite
        .tests
        .iter()
        .map(|t| (t.def.test.clone(), &t.def))
        .collect();

    enum Item<'s> {
        Test(&'s vault_dsl::LoadedTest),
        Flow(&'s vault_dsl::LoadedFlow),
    }
    let Plan { flows, standalone } = plan;
    let mut items: Vec<Item> = flows
        .into_iter()
        .map(Item::Flow)
        .chain(standalone.into_iter().map(Item::Test))
        .collect();

    if args.shuffle {
        let seed = args.seed.unwrap_or_else(rand::random);
        println!("{} shuffle seed: {seed}", "→".cyan());
        use rand::{seq::SliceRandom, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        items.shuffle(&mut rng);
    }

    let started = std::time::Instant::now();
    let mut run = RunResult {
        schema_version: 1,
        environment: args.env.clone(),
        tests: vec![],
        duration_ms: 0,
    };

    let empty = IndexMap::new();
    for item in items {
        if runner.abort_run.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        match item {
            Item::Test(t) => {
                let result = runner.run_test(&t.def, &empty, true, None, None).await;
                run.tests.push(result);
            }
            Item::Flow(f) => {
                let outcome = runner.run_flow(&f.def, &tests_by_name).await;
                run.tests.extend(outcome.results);
            }
        }
    }
    run.duration_ms = started.elapsed().as_millis() as u64;

    vault_report::print_run(&run, args.verbose);

    let run_exit_code = run.exit_code();
    let json_path = args.report.or(suite.config.report.json.clone());
    let junit_path = args.junit.or(suite.config.report.junit.clone());
    let report_failures = write_requested_reports(&run, json_path, junit_path);
    for failure in &report_failures {
        eprintln!(
            "{} could not write {} report to {}: {}",
            "report error:".red().bold(),
            failure.format,
            failure.path,
            failure.error
        );
    }

    report_aware_exit_code(run_exit_code, !report_failures.is_empty())
}
