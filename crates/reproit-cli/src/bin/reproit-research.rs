use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Parser, Subcommand};
use reproit_cli::render::{
    PublicErrorContext, parse_cli, render_error, render_observed_results, stderr_line, stdout_line,
};
use reproit_core::{Error, ErrorCode};

#[derive(Parser)]
#[command(
    name = "reproit-research",
    bin_name = "reproit-research",
    version,
    disable_help_subcommand = true,
    about = "Run verified experiments and evaluations."
)]
struct Cli {
    /// Show error codes, failed criteria, and execution problems.
    #[arg(long, global = true)]
    details: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verify and save an authored research experiment.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    Add { definition_path: PathBuf },
    /// Compare baseline and proposed outputs and save release evidence.
    Gate(GateArgs),
    /// Verify a saved release evidence bundle offline.
    Verify { bundle_path: PathBuf },
}

#[derive(Args)]
struct GateArgs {
    #[arg(long)]
    config: PathBuf,
}

fn main() -> ExitCode {
    let cli = match parse_cli::<Cli>("reproit-research") {
        Ok(cli) => cli,
        Err(exit) => return exit,
    };
    let details = cli.details;
    match cli.command {
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        Command::Add { definition_path } => command_exit(add_command(&definition_path), details),
        Command::Gate(args) => {
            release_command_exit(reproit_cli::release_gate::run(&args.config), details, true)
        }
        Command::Verify { bundle_path } => release_command_exit(
            reproit_cli::release_gate::verify(&bundle_path),
            details,
            false,
        ),
    }
}

fn add_command(definition_path: &std::path::Path) -> Result<(), Error> {
    let root = std::env::current_dir().map_err(|_| evaluation_error())?;
    let input = reproit_cli::authored_repro::AddReproInput {
        definition_path: definition_path.to_string_lossy().into_owned(),
    };
    let result = reproit_cli::authored_repro::add_repro(&root, &input)?;
    stdout_line(format_args!("Added {}.", result.repro_id))?;
    stdout_line(format_args!("Claim {}.", result.claim_digest))?;
    render_observed_results(&result.observed_results)?;
    stdout_line(format_args!("Run '{}'.", result.next_command))
}

fn command_exit(result: Result<(), Error>, details: bool) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            render_error(PublicErrorContext::General, &error, details);
            ExitCode::from(2)
        }
    }
}

fn release_command_exit(
    result: Result<reproit_cli::release_gate::ReleaseReport, Error>,
    details: bool,
    print_unknown_on_error: bool,
) -> ExitCode {
    match result {
        Ok(report) => {
            let (label, code) = match report.decision {
                reproit_experiments::ReleaseDecision::Pass => ("PASS", 0),
                reproit_experiments::ReleaseDecision::Regression => ("REGRESSION", 1),
                reproit_experiments::ReleaseDecision::Unknown => ("UNKNOWN", 2),
            };
            if stdout_line(format_args!("{label}")).is_err() {
                return ExitCode::from(2);
            }
            if details {
                report.render_details();
            } else if code != 0 {
                stderr_line(format_args!(
                    "Run with --details to see failed criteria and execution problems."
                ));
            }
            ExitCode::from(code)
        }
        Err(error) => {
            if print_unknown_on_error {
                let _ = stdout_line(format_args!("UNKNOWN"));
            }
            render_error(PublicErrorContext::Release, &error, details);
            ExitCode::from(2)
        }
    }
}

fn evaluation_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not evaluate the command.",
    )
}
