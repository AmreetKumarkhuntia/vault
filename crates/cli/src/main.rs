mod gate;
mod preflight;
mod runcmd;

use std::ffi::OsString;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "vault",
    version,
    about = "Black-box HTTP API test harness: YAML-defined tests with optional state stores, a recording dependency mock, and verification that names exactly what is missing.",
    after_help = "Quick run: vault --run <SUITE_DIR|vault.yaml> [RUN_OPTIONS]"
)]
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse_from(normalize_quick_run(std::env::args_os()));
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
        } => runcmd::list(pattern, tag, suite_dir),
        Command::Validate { suite_dir } => runcmd::validate(suite_dir),
        Command::Env { env, suite_dir } => runcmd::print_env(env, suite_dir),
    };
    std::process::exit(code);
}

/// Keep the regular clap subcommand as the single source of truth while
/// supporting the npm-friendly `vault --run <suite>` shorthand.
fn normalize_quick_run(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.into_iter().collect();
    if args.get(1).is_some_and(|arg| arg == "--run") {
        args[1] = OsString::from("run");
        args.insert(2, OsString::from("--suite-dir"));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::normalize_quick_run;
    use std::ffi::OsString;

    fn strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn expands_quick_run_and_preserves_following_options() {
        assert_eq!(
            normalize_quick_run(strings(&[
                "vault",
                "--run",
                "tests/http-only",
                "--tag",
                "smoke",
            ])),
            strings(&[
                "vault",
                "run",
                "--suite-dir",
                "tests/http-only",
                "--tag",
                "smoke",
            ])
        );
    }

    #[test]
    fn leaves_existing_subcommands_unchanged() {
        let args = strings(&["vault", "run", "--suite-dir", "tests/flows"]);
        assert_eq!(normalize_quick_run(args.clone()), args);
    }
}
