mod gate;
mod preflight;
mod runcmd;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vault", version, about = "Black-box HTTP API test harness: YAML-defined tests with seeded Postgres/Redis, a recording dependency mock, and verification that names exactly what is missing.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run tests and flows
    Run {
        /// Glob over test/flow names, e.g. 'orders*' or a full name
        pattern: Option<String>,
        #[arg(long, short, action = clap::ArgAction::Append)]
        tag: Vec<String>,
        #[arg(long, default_value = "local")]
        env: String,
        #[arg(long, default_value = "tests")]
        suite_dir: String,
        /// Pause at lifecycle boundaries for interactive inspection
        #[arg(long)]
        step: bool,
        /// Shuffle execution order (order-independence audit)
        #[arg(long)]
        shuffle: bool,
        #[arg(long)]
        seed: Option<u64>,
        /// Write the JSON report here (overrides config)
        #[arg(long)]
        report: Option<String>,
        /// Write JUnit XML here (overrides config)
        #[arg(long)]
        junit: Option<String>,
        #[arg(long, short)]
        verbose: bool,
    },
    /// Show the resolved run plan without executing
    List {
        pattern: Option<String>,
        #[arg(long, short, action = clap::ArgAction::Append)]
        tag: Vec<String>,
        #[arg(long, default_value = "tests")]
        suite_dir: String,
    },
    /// Parse and statically validate the whole suite
    Validate {
        #[arg(long, default_value = "tests")]
        suite_dir: String,
    },
    /// Print the environment the target should be started with
    Env {
        #[arg(long, default_value = "local")]
        env: String,
        #[arg(long, default_value = "tests")]
        suite_dir: String,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let code = match cli.command {
        Command::Run { pattern, tag, env, suite_dir, step, shuffle, seed, report, junit, verbose } => {
            runcmd::run(runcmd::RunArgs {
                pattern,
                tags: tag,
                env,
                suite_dir,
                step,
                shuffle,
                seed,
                report,
                junit,
                verbose,
            })
        }
        Command::List { pattern, tag, suite_dir } => runcmd::list(pattern, tag, suite_dir),
        Command::Validate { suite_dir } => runcmd::validate(suite_dir),
        Command::Env { env, suite_dir } => runcmd::print_env(env, suite_dir),
    };
    std::process::exit(code);
}
