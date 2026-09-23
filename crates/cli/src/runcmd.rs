use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;
use owo_colors::OwoColorize;
use serde_json::{json, Value};
use vault_core::{NoopGate, RunResult, TestRunner};
use vault_dsl::Suite;
use vault_mock::MockServer;
use vault_store::{StoreConnConfig, StoreRegistry};

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

fn load_suite(suite_dir: &str) -> Result<Suite, i32> {
    match vault_dsl::discover(Path::new(suite_dir)) {
        Ok(s) => Ok(s),
        Err(e) => {
            eprintln!("{} {e}", "config error:".red().bold());
            Err(2)
        }
    }
}

fn validate_all(suite: &Suite, reg: &StoreRegistry) -> Vec<String> {
    let mut issues: Vec<String> = vault_dsl::validate_suite(suite)
        .into_iter()
        .map(|i| i.to_string())
        .collect();

    for t in &suite.tests {
        let origin = t.path.display().to_string();
        for (kind, doc) in &t.def.seed {
            match reg.get(kind) {
                Some(driver) => {
                    if let Err(e) = driver.validate(doc, vault_store::DocMode::Seed) {
                        issues.push(format!("{origin}: {e}"));
                    }
                }
                None => issues.push(format!("{origin}: unknown store `{kind}` in seed:")),
            }
        }
        for (kind, doc) in &t.def.verify.stores {
            match reg.get(kind) {
                Some(driver) => {
                    if let Err(e) = driver.validate(doc, vault_store::DocMode::Verify) {
                        issues.push(format!("{origin}: {e}"));
                    }
                }
                None => issues.push(format!("{origin}: unknown store `{kind}` in verify:")),
            }
        }
    }
    issues
}

struct Plan<'s> {
    standalone: Vec<&'s vault_dsl::LoadedTest>,
    flows: Vec<&'s vault_dsl::LoadedFlow>,
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

pub fn list(pattern: Option<String>, tags: Vec<String>, suite_dir: String) -> i32 {
    let suite = match load_suite(&suite_dir) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let plan = match plan(&suite, &pattern, &tags) {
        Ok(p) => p,
        Err(c) => return c,
    };
    for f in &plan.flows {
        println!("{} {}", "flow".cyan().bold(), f.def.flow);
        for (i, stage) in f.def.stages.iter().enumerate() {
            println!("    {}. {}", i + 1, stage.test);
        }
    }
    for t in &plan.standalone {
        let tags = if t.def.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", t.def.tags.join(", "))
        };
        println!("{} {}{}", "test".green().bold(), t.def.test, tags.dimmed());
    }
    println!(
        "\n{} flows, {} standalone tests",
        plan.flows.len(),
        plan.standalone.len()
    );
    0
}

pub fn validate(suite_dir: String) -> i32 {
    let suite = match load_suite(&suite_dir) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let issues = validate_all(&suite, &registry());
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
    let suite = match load_suite(&args.suite_dir) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let reg = registry();
    let issues = validate_all(&suite, &reg);
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
    let Some(environment) = suite.config.environments.get(&args.env).cloned() else {
        eprintln!(
            "{} unknown environment `{}`",
            "config error:".red().bold(),
            args.env
        );
        return 2;
    };
    if args.step && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!(
            "{} --step needs an interactive terminal",
            "usage error:".red().bold()
        );
        return 2;
    }

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async { run_async(args, suite, reg, environment).await })
}

async fn run_async(
    args: RunArgs,
    suite: Suite,
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

    let plan = match plan(&suite, &args.pattern, &args.tags) {
        Ok(p) => p,
        Err(c) => return c,
    };

    let gate: Arc<dyn vault_core::Gate> = if args.step {
        Arc::new(InteractiveGate::new(stores.clone()))
    } else {
        Arc::new(NoopGate)
    };

    let runner = TestRunner {
        client: reqwest::Client::new(),
        target_base_url: environment.target.base_url.clone(),
        defaults: suite.config.defaults.clone(),
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
    let mut items: Vec<Item> = plan
        .flows
        .iter()
        .map(|f| Item::Flow(f))
        .chain(plan.standalone.iter().map(|t| Item::Test(t)))
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

    let json_path = args.report.or(suite.config.report.json.clone());
    if let Some(path) = json_path {
        match vault_report::write_json(&run, &path) {
            Ok(()) => println!("{} JSON report: {path}", "→".cyan()),
            Err(e) => eprintln!("{} could not write {path}: {e}", "warning:".yellow()),
        }
    }
    let junit_path = args.junit.or(suite.config.report.junit.clone());
    if let Some(path) = junit_path {
        match vault_report::write_junit(&run, &path) {
            Ok(()) => println!("{} JUnit report: {path}", "→".cyan()),
            Err(e) => eprintln!("{} could not write {path}: {e}", "warning:".yellow()),
        }
    }

    run.exit_code()
}
