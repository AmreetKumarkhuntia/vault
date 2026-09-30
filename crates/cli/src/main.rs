#![deny(clippy::print_stderr, clippy::print_stdout)]

mod gate;
mod preflight;
mod runcmd;

use std::ffi::OsString;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "vault",
    version,
    about = "Black-box HTTP API test harness: YAML-defined tests with optional state stores, a recording dependency mock, and verification that names exactly what is missing.",
    after_help = "Quick run: vault --run <SUITE_DIR|vault.yaml> [RUN_OPTIONS]"
)]
struct Cli {
    /// Disable ANSI colors in all terminal output
    #[arg(long, global = true)]
    no_color: bool,

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
        #[arg(long, default_value = "tests/flows")]
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
        #[arg(long, default_value = "tests/flows")]
        suite_dir: String,
        /// Show the resolved SQL fixture files in execution order
        #[arg(long)]
        fixtures: bool,
    },
    /// Parse and statically validate the whole suite
    Validate {
        #[arg(long, default_value = "tests/flows")]
        suite_dir: String,
    },
    /// Print the environment the target should be started with
    Env {
        #[arg(long, default_value = "local")]
        env: String,
        #[arg(long, default_value = "tests/flows")]
        suite_dir: String,
    },
}

fn main() {
    let args = normalize_quick_run(std::env::args_os());
    let disable_color = color_disabled(&args);
    if disable_color {
        anstream::ColorChoice::Never.write_global();
    }

    let clap_color = if disable_color {
        clap::ColorChoice::Never
    } else {
        clap::ColorChoice::Auto
    };
    let matches = Cli::command().color(clap_color).get_matches_from(args);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());

    tracing_subscriber::fmt()
        .with_writer(anstream::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let code = match cli.command {
        Command::Run {
            pattern,
            tag,
            env,
            suite_dir,
            step,
            shuffle,
            seed,
            report,
            junit,
            verbose,
        } => runcmd::run(runcmd::RunArgs {
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
        }),
        Command::List {
            pattern,
            tag,
            suite_dir,
            fixtures,
        } => runcmd::list(pattern, tag, suite_dir, fixtures),
        Command::Validate { suite_dir } => runcmd::validate(suite_dir),
        Command::Env { env, suite_dir } => runcmd::print_env(env, suite_dir),
    };
    std::process::exit(code);
}

/// Keep the regular clap subcommand as the single source of truth while
/// supporting the npm-friendly `vault --run <suite>` shorthand.
fn normalize_quick_run(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.into_iter().collect();
    let mut index = 1;
    while args.get(index).is_some_and(|arg| arg == "--no-color") {
        index += 1;
    }
    if args.get(index).is_some_and(|arg| arg == "--run") {
        args[index] = OsString::from("run");
        args.insert(index + 1, OsString::from("--suite-dir"));
    }
    args
}

fn has_no_color(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--no-color")
}

fn color_disabled(args: &[OsString]) -> bool {
    has_no_color(args)
        || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
        || std::env::var_os("CLICOLOR").is_some_and(|value| value == "0")
}
