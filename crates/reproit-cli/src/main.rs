use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

mod capture_detection;
mod capture_probe;
mod go_capture_probe;
mod go_instrumentation;
mod login_command;

#[cfg(test)]
use login_command::select_login_configuration;

use clap::{
    Args, Parser, Subcommand, ValueEnum,
    error::{ContextKind, ContextValue, ErrorKind},
};
use reproit_app::{
    InitializationResult, ProjectStore,
    agent::{
        AgentOperations as _, CheckReprosInput, CheckStatus as AgentCheckStatus, GetReproInput,
        ListReprosInput, ListReprosResult, ReproScope, TriageReproInput,
    },
    initialize, remove_kept,
};
use reproit_backend::config::{BackendSdk, ProjectConfig, ProjectSourceConfig, RunSpec};
use reproit_cli::agent::ProductionAgent;
use reproit_cli::cloud::HttpCloudClient;
use reproit_cli::render::{PublicErrorContext, render_error, stderr_line, stdout_line};
use reproit_cli::{
    FilesystemRepository, NativeCredentialStore, current_git_repository,
    initialization::{InitializationDirectory, InitializationService},
};
use reproit_cloud_api::{
    FuzzCampaignCreate, FuzzCampaignGrant, Priority, ServiceCatalogQuery, Workflow,
};
use reproit_core::{
    Error, ErrorCode, canonical,
    identity::{FuzzCampaignId, ReproId},
    model::{FuzzCampaignState, Validate},
};
use secrecy::ExposeSecret as _;
use serde::Serialize;

use capture_detection::{ReleasedSdkDeclaration, normalize_startup_run, released_sdk};

const OFFICIAL_CLOUD_ORIGIN: &str = "https://cloud.reproit.com";
const SERVICE_CATALOG_PAGE_SIZE: u8 = 50;
const MAX_SERVICE_CATALOG_PAGES: usize = 6;
const CAMPAIGN_GRANT_STATE_DIRECTORY: &str = "fuzz-campaigns";
const FUZZER_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(
    name = "reproit",
    // clap derives the usage name from argv[0] at runtime, which is
    // "reproit.exe" on Windows. The public help corpus is one exact
    // cross-platform contract, so the binary name is pinned.
    bin_name = "reproit",
    version,
    disable_help_subcommand = true,
    about = "Reproduce, fix, and keep production bugs."
)]
struct Cli {
    #[arg(long, global = true)]
    details: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Campaign(CampaignArgs),
    Login,
    Init(InitArgs),
    List(ListArgs),
    Triage(TriageArgs),
    Debug { repro_id: ReproId },
    Check { repro_id: Option<ReproId> },
    Gate(GateArgs),
    Keep { repro_id: ReproId },
    Mcp,
    Remove { repro_id: ReproId },
    Verify { bundle_path: PathBuf },
}

#[derive(Args)]
struct CampaignArgs {
    #[command(subcommand)]
    command: CampaignCommand,
}

#[derive(Subcommand)]
enum CampaignCommand {
    Cancel { campaign_id: FuzzCampaignId },
    Create { path: PathBuf },
    Status { campaign_id: FuzzCampaignId },
    Validate { path: PathBuf },
}

#[derive(Serialize)]
struct FuzzerLaunchRequest {
    bearer_token: String,
    campaign_grant: FuzzCampaignGrant,
    cloud_origin: String,
    format: &'static str,
    production_authorization: Option<String>,
    seed: u64,
}

#[derive(Args)]
struct GateArgs {
    #[arg(long)]
    config: PathBuf,
}

#[derive(Args)]
struct InitArgs {
    #[arg(long)]
    non_interactive: bool,
    #[arg(long)]
    service: Option<String>,
    #[arg(long, value_enum)]
    sdk: Option<SdkArg>,
    #[arg(long)]
    service_path: Option<String>,
    #[arg(last = true, num_args = 1..)]
    run: Vec<String>,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long)]
    kept: bool,
    #[arg(long)]
    all: bool,
    #[arg(long)]
    assignee: Option<String>,
    #[arg(long)]
    priority: Option<PriorityArg>,
}

#[derive(Args)]
struct TriageArgs {
    repro_id: ReproId,
    #[arg(long)]
    priority: Option<PriorityArg>,
    #[arg(long, conflicts_with = "unassign")]
    assign: Option<String>,
    #[arg(long)]
    unassign: bool,
    #[arg(long, conflicts_with = "reopen")]
    resolve: bool,
    #[arg(long)]
    reopen: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum PriorityArg {
    Unset,
    P0,
    P1,
    P2,
    P3,
}

#[derive(Clone, Copy, ValueEnum)]
enum SdkArg {
    Dotnet,
    Go,
    Nodejs,
    Python,
    Rust,
}

impl From<SdkArg> for BackendSdk {
    fn from(value: SdkArg) -> Self {
        match value {
            SdkArg::Dotnet => Self::Dotnet,
            SdkArg::Go => Self::Go,
            SdkArg::Nodejs => Self::Nodejs,
            SdkArg::Python => Self::Python,
            SdkArg::Rust => Self::Rust,
        }
    }
}

impl From<PriorityArg> for Priority {
    fn from(value: PriorityArg) -> Self {
        match value {
            PriorityArg::Unset => Self::Unset,
            PriorityArg::P0 => Self::P0,
            PriorityArg::P1 => Self::P1,
            PriorityArg::P2 => Self::P2,
            PriorityArg::P3 => Self::P3,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if go_instrumentation::is_invocation(&arguments) {
        return go_instrumentation::run(&arguments);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
        // A command parsing failure is not a Repro evaluation failure. It
        // gets a bounded command error that never echoes unsanitized input.
        Err(error) if error.kind() == ErrorKind::InvalidSubcommand => {
            stderr_line(format_args!(
                "error: unrecognized command '{}'",
                unrecognized_command_name(&error)
            ));
            stderr_line(format_args!("Run 'reproit --help' for usage."));
            return ExitCode::from(2);
        }
        Err(_) => {
            stderr_line(format_args!("error: invalid command usage"));
            stderr_line(format_args!("Run 'reproit --help' for usage."));
            return ExitCode::from(2);
        }
    };
    let details = cli.details;
    let context = PublicErrorContext::from(&cli.command);
    match &cli.command {
        Command::Gate(args) => {
            return release_command_exit(
                reproit_cli::release_gate::run(&args.config),
                details,
                true,
            );
        }
        Command::Verify { bundle_path } => {
            return release_command_exit(
                reproit_cli::release_gate::verify(bundle_path),
                details,
                false,
            );
        }
        _ => {}
    }
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            render_error(context, &error, details);
            ExitCode::from(error_exit_code(context, &error))
        }
    }
}

fn release_command_exit(
    result: Result<reproit_experiments::ReleaseDecision, Error>,
    details: bool,
    print_unknown_on_error: bool,
) -> ExitCode {
    match result {
        Ok(decision) => {
            let (label, code) = match decision {
                reproit_experiments::ReleaseDecision::Pass => ("PASS", 0),
                reproit_experiments::ReleaseDecision::Regression => ("REGRESSION", 1),
                reproit_experiments::ReleaseDecision::Unknown => ("UNKNOWN", 2),
            };
            if stdout_line(format_args!("{label}")).is_err() {
                return ExitCode::from(2);
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

fn error_exit_code(context: PublicErrorContext, error: &Error) -> u8 {
    if matches!(context, PublicErrorContext::Check) && error.code == ErrorCode::DifferentFailure {
        1
    } else {
        2
    }
}

async fn run(cli: Cli) -> Result<(), Error> {
    let root = std::env::current_dir().map_err(|_| evaluation_error())?;
    let mut store = FilesystemRepository::new(root.clone());
    let agent = ProductionAgent::new(root.clone());
    match cli.command {
        Command::Campaign(args) => campaign_command(args).await,
        Command::Login => login_command::run().await,
        Command::Init(args) => initialize_command(&root, args).await,
        Command::List(args) => list_command(&agent, args).await,
        Command::Triage(args) => triage_command(&agent, args).await,
        Command::Remove { repro_id } => {
            remove_kept(&mut store, repro_id)?;
            stdout_line(format_args!("Removed the kept reference for {repro_id}."))?;
            stdout_line(format_args!("Cloud history was not deleted."))?;
            Ok(())
        }
        Command::Debug { repro_id } => debug_command(&agent, repro_id).await,
        Command::Check { repro_id } => check_command(&agent, repro_id).await,
        Command::Gate(_) | Command::Verify { .. } => Err(evaluation_error()),
        Command::Keep { repro_id } => keep_command(&agent, repro_id).await,
        Command::Mcp => reproit_cli::mcp::serve(root).await,
    }
}

async fn campaign_command(args: CampaignArgs) -> Result<(), Error> {
    match args.command {
        CampaignCommand::Validate { path } => {
            run_fuzzer_command("validate", &path)?;
            stdout_line(format_args!("The campaign is valid."))
        }
        CampaignCommand::Create { path } => create_campaign_command(&path).await,
        CampaignCommand::Status { campaign_id } => {
            let status = cloud_client()?.get_fuzz_campaign(campaign_id).await?;
            if matches!(
                status.state,
                FuzzCampaignState::Complete | FuzzCampaignState::Cancelled
            ) {
                remove_campaign_grant_state(status.campaign_id)?;
            }
            stdout_line(format_args!(
                "{}\t{}\t{} scheduled\t{} found\t{} verified",
                status.campaign_id,
                campaign_state_label(status.state),
                status.cases_scheduled,
                status.cases_found,
                status.cases_verified
            ))
        }
        CampaignCommand::Cancel { campaign_id } => {
            let status = cloud_client()?.cancel_fuzz_campaign(campaign_id).await?;
            remove_campaign_grant_state(status.campaign_id)?;
            stdout_line(format_args!(
                "Campaign {} is {}.",
                status.campaign_id,
                campaign_state_label(status.state)
            ))
        }
    }
}

const fn campaign_state_label(state: FuzzCampaignState) -> &'static str {
    match state {
        FuzzCampaignState::Created => "CREATED",
        FuzzCampaignState::Running => "RUNNING",
        FuzzCampaignState::Stopping => "STOPPING",
        FuzzCampaignState::Complete => "COMPLETE",
        FuzzCampaignState::Cancelled => "CANCELLED",
    }
}

async fn create_campaign_command(path: &Path) -> Result<(), Error> {
    let description = run_fuzzer_command("describe", path)?;
    let request: FuzzCampaignCreate = canonical::parse_strict(&description)?;
    if canonical::canonical_bytes(&request)? != description {
        return Err(Error::schema_invalid());
    }
    let fuzzer_program = fuzzer_program()?;
    let production_authorization = optional_secret("REPROIT_FUZZ_PRODUCTION_CAPABILITY")?;
    let mut seed_bytes = [0_u8; 8];
    getrandom::fill(&mut seed_bytes).map_err(|_| evaluation_error())?;
    let (cloud, origin, bearer_token) = campaign_cloud_client()?;
    let created = cloud
        .create_fuzz_campaign(request.project_id, &request)
        .await?;
    let launch = FuzzerLaunchRequest {
        bearer_token,
        campaign_grant: created.campaign_grant.clone(),
        cloud_origin: origin,
        format: "reproit.fuzz-launch.v1",
        production_authorization,
        seed: u64::from_le_bytes(seed_bytes),
    };
    let launch_bytes = match canonical::canonical_bytes(&launch) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = cloud.cancel_fuzz_campaign(created.campaign_id).await;
            return Err(error);
        }
    };
    if launch_bytes.len() > 32 * 1_024 {
        let _ = cloud.cancel_fuzz_campaign(created.campaign_id).await;
        return Err(Error::new(
            ErrorCode::RuntimeQuota,
            "The campaign launch request exceeds its byte limit.",
        ));
    }
    if let Err(error) = save_campaign_grant_state(&created.campaign_grant) {
        let _ = cloud.cancel_fuzz_campaign(created.campaign_id).await;
        return Err(error);
    }
    let Ok(mut child) = ProcessCommand::new(fuzzer_program)
        .arg("run")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    else {
        let _ = remove_campaign_grant_state(created.campaign_id);
        let _ = cloud.cancel_fuzz_campaign(created.campaign_id).await;
        return Err(evaluation_error());
    };
    let write_result = child
        .stdin
        .take()
        .ok_or_else(evaluation_error)
        .and_then(|mut stdin| {
            stdin
                .write_all(&launch_bytes)
                .map_err(|_| evaluation_error())
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        let _ = remove_campaign_grant_state(created.campaign_id);
        let _ = cloud.cancel_fuzz_campaign(created.campaign_id).await;
        return Err(error);
    }
    stdout_line(format_args!("Campaign {} created.", created.campaign_id))?;
    stdout_line(format_args!("Local fuzzer process {} started.", child.id()))
}

fn campaign_grant_state_path(campaign_id: FuzzCampaignId) -> Result<PathBuf, Error> {
    let home = std::env::home_dir().ok_or_else(evaluation_error)?;
    Ok(campaign_grant_state_path_from_home(&home, campaign_id))
}

fn campaign_grant_state_path_from_home(home: &Path, campaign_id: FuzzCampaignId) -> PathBuf {
    #[cfg(target_os = "macos")]
    let state_root = home
        .join("Library")
        .join("Application Support")
        .join("ReproIt");
    #[cfg(target_os = "windows")]
    let state_root = home.join("AppData").join("Local").join("ReproIt");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let state_root = home.join(".local").join("state").join("reproit");

    state_root
        .join(CAMPAIGN_GRANT_STATE_DIRECTORY)
        .join(format!("{campaign_id}.json"))
}

fn save_campaign_grant_state(grant: &FuzzCampaignGrant) -> Result<(), Error> {
    grant.validate()?;
    let path = campaign_grant_state_path(grant.campaign_id)?;
    write_campaign_grant_state(&path, grant)
}

fn write_campaign_grant_state(path: &Path, grant: &FuzzCampaignGrant) -> Result<(), Error> {
    let parent = path.parent().ok_or_else(evaluation_error)?;
    fs::create_dir_all(parent).map_err(|_| evaluation_error())?;
    #[cfg(unix)]
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|_| evaluation_error())?;

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|_| {
        Error::new(
            ErrorCode::ConfigConflict,
            "The campaign grant state already exists or cannot be created.",
        )
    })?;
    let bytes = canonical::canonical_bytes(grant)?;
    if file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .is_err()
    {
        let _ = fs::remove_file(path);
        return Err(Error::new(
            ErrorCode::EvaluationError,
            "Repro It could not save the campaign grant state.",
        ));
    }
    Ok(())
}

fn remove_campaign_grant_state(campaign_id: FuzzCampaignId) -> Result<(), Error> {
    let path = campaign_grant_state_path(campaign_id)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Error::new(
            ErrorCode::EvaluationError,
            "Repro It could not remove the campaign grant state.",
        )),
    }
}

fn run_fuzzer_command(operation: &str, path: &Path) -> Result<Vec<u8>, Error> {
    run_bounded_fuzzer_command(&fuzzer_program()?, operation, path, FUZZER_CONTROL_TIMEOUT)
}

fn run_bounded_fuzzer_command(
    program: &Path,
    operation: &str,
    path: &Path,
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    if timeout.is_zero() {
        return Err(fuzzer_control_timeout());
    }
    let mut child = ProcessCommand::new(program)
        .arg(operation)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| evaluation_error())?;
    let stdout = child.stdout.take().ok_or_else(evaluation_error)?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.take(65_537).read_to_end(&mut output)?;
        Ok::<_, std::io::Error>(output)
    });
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(fuzzer_control_timeout)?;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|_| evaluation_error())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(fuzzer_control_timeout());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut output = reader
        .join()
        .map_err(|_| evaluation_error())?
        .map_err(|_| evaluation_error())?;
    if !status.success() || output.len() > 65_536 {
        return Err(Error::new(
            ErrorCode::SchemaInvalid,
            "The campaign file is invalid.",
        ));
    }
    while output.last() == Some(&b'\n') || output.last() == Some(&b'\r') {
        output.pop();
    }
    Ok(output)
}

fn fuzzer_control_timeout() -> Error {
    Error::new(
        ErrorCode::RuntimeQuota,
        "The campaign validator reached its execution limit.",
    )
}

fn fuzzer_program() -> Result<PathBuf, Error> {
    match std::env::var_os("REPROIT_FUZZER_PATH") {
        Some(path) if !path.is_empty() => Ok(PathBuf::from(path)),
        Some(_) => Err(evaluation_error()),
        None => {
            let executable = std::env::current_exe().map_err(|_| evaluation_error())?;
            let name = if cfg!(windows) {
                "reproit-fuzzer.exe"
            } else {
                "reproit-fuzzer"
            };
            Ok(executable.parent().ok_or_else(evaluation_error)?.join(name))
        }
    }
}

fn campaign_cloud_client() -> Result<(HttpCloudClient, String, String), Error> {
    let session = NativeCredentialStore::open()?.load()?;
    let origin = match std::env::var("REPROIT_CLOUD_ORIGIN") {
        Ok(origin) => origin,
        Err(std::env::VarError::NotPresent) => OFFICIAL_CLOUD_ORIGIN.to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => return Err(Error::schema_invalid()),
    };
    let bearer_token = session.expose_secret().to_owned();
    let cloud = HttpCloudClient::new(&origin, session)?;
    Ok((cloud, origin, bearer_token))
}

fn optional_secret(name: &str) -> Result<Option<String>, Error> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() && value.len() <= 16_384 => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => Err(Error::schema_invalid()),
        Err(std::env::VarError::NotPresent) => Ok(None),
    }
}

async fn debug_command(agent: &ProductionAgent, repro_id: ReproId) -> Result<(), Error> {
    let result = agent.debug(repro_id).await?;
    match result.result {
        reproit_core::model::ExecutionOutcome::TargetReproduced => {
            stdout_line(format_args!("Failure reproduced."))?;
            Ok(())
        }
        reproit_core::model::ExecutionOutcome::TargetAbsent => {
            stdout_line(format_args!("Failure not reproduced."))?;
            Ok(())
        }
        _ => Err(evaluation_error()),
    }
}

async fn check_command(agent: &ProductionAgent, repro_id: Option<ReproId>) -> Result<(), Error> {
    let is_set_check = repro_id.is_none();
    let result = agent
        .check_repros_streaming(
            CheckReprosInput {
                repro_ids: repro_id.into_iter().collect(),
            },
            |check| {
                let label = match check.status {
                    AgentCheckStatus::Pass => "PASS",
                    AgentCheckStatus::Regression => "REGRESSION",
                    AgentCheckStatus::Error => "ERROR",
                };
                stdout_line(format_args!("{label} {}", check.repro_id))
            },
        )
        .await?;
    if is_set_check {
        stdout_line(format_args!(
            "Totals: {} passed, {} regressed, {} errors.",
            result.pass_count, result.regression_count, result.error_count
        ))?;
    }
    if result.error_count > 0 {
        return Err(result
            .errors
            .first()
            .and_then(|check| check.error.clone())
            .unwrap_or_else(evaluation_error));
    }
    if result.regression_count > 0 {
        return Err(Error::new(
            ErrorCode::DifferentFailure,
            "At least one Repro regressed.",
        ));
    }
    Ok(())
}

async fn keep_command(agent: &ProductionAgent, repro_id: ReproId) -> Result<(), Error> {
    let result = agent.keep_repro(GetReproInput { repro_id }).await?;
    stdout_line(format_args!("Kept {repro_id}."))?;
    stdout_line(format_args!(
        "Commit {} with your fix.",
        result.tracked_reference_path
    ))?;
    Ok(())
}

async fn list_command(agent: &ProductionAgent, args: ListArgs) -> Result<(), Error> {
    let result = agent
        .list_repros(ListReprosInput {
            assignee_id: args.assignee,
            cursor: None,
            limit: 100,
            priority: args.priority.map(Into::into).into_iter().collect(),
            scope: if args.kept {
                ReproScope::Kept
            } else {
                ReproScope::Cloud
            },
            workflow: if args.all || args.kept {
                Vec::new()
            } else {
                vec![Workflow::Open, Workflow::Regressed]
            },
        })
        .await?;
    match result {
        ListReprosResult::Cloud { repros, .. } => {
            if repros.is_empty() {
                stdout_line(format_args!("No open Repros."))?;
                return Ok(());
            }
            for repro in repros {
                let source = if let Some(campaign_id) = repro.campaign_id {
                    format!("Fuzz campaign discovered ({campaign_id})")
                } else {
                    "Production discovered".to_owned()
                };
                stdout_line(format_args!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    repro.repro_id,
                    priority_label(repro.priority),
                    workflow_label(repro.workflow),
                    repro.assignee_id.as_deref().unwrap_or("UNASSIGNED"),
                    repro.failure_summary.type_name,
                    source
                ))?;
            }
        }
        ListReprosResult::Kept { repros, .. } => {
            if repros.is_empty() {
                stdout_line(format_args!("No kept Repros."))?;
                return Ok(());
            }
            for repro in repros {
                stdout_line(format_args!(
                    "{}\t{}",
                    repro.repro_id, repro.tracked_reference_path
                ))?;
            }
        }
    }
    Ok(())
}

async fn triage_command(agent: &ProductionAgent, args: TriageArgs) -> Result<(), Error> {
    let detail = agent
        .get_repro(GetReproInput {
            repro_id: args.repro_id,
        })
        .await?;
    let assignee_id = match (args.assign, args.unassign) {
        (Some(assignee), false) => Some(assignee),
        (None, true) => None,
        (None, false) => detail.summary.assignee_id,
        (Some(_), true) => return Err(Error::schema_invalid()),
    };
    let change = agent
        .triage_repro(TriageReproInput {
            assignee_id,
            priority: args.priority.map_or(detail.summary.priority, Into::into),
            repro_id: args.repro_id,
            triage_revision: detail.summary.triage_revision,
            workflow: if args.resolve {
                Workflow::Resolved
            } else if args.reopen {
                Workflow::Open
            } else {
                detail.summary.workflow
            },
        })
        .await?;
    if change.previous.priority != change.current.priority {
        stdout_line(format_args!(
            "Priority: {} -> {}",
            priority_label(change.previous.priority),
            priority_label(change.current.priority)
        ))?;
    }
    if change.previous.assignee_id != change.current.assignee_id {
        stdout_line(format_args!(
            "Assignee: {} -> {}",
            change
                .previous
                .assignee_id
                .as_deref()
                .unwrap_or("UNASSIGNED"),
            change
                .current
                .assignee_id
                .as_deref()
                .unwrap_or("UNASSIGNED")
        ))?;
    }
    if change.previous.workflow != change.current.workflow {
        stdout_line(format_args!(
            "Workflow: {} -> {}",
            workflow_label(change.previous.workflow),
            workflow_label(change.current.workflow)
        ))?;
    }
    Ok(())
}

const fn priority_label(priority: Priority) -> &'static str {
    match priority {
        Priority::Unset => "UNSET",
        Priority::P0 => "P0",
        Priority::P1 => "P1",
        Priority::P2 => "P2",
        Priority::P3 => "P3",
    }
}

const fn workflow_label(workflow: Workflow) -> &'static str {
    match workflow {
        Workflow::Open => "OPEN",
        Workflow::Resolved => "RESOLVED",
        Workflow::Regressed => "REGRESSED",
    }
}

fn cloud_client() -> Result<HttpCloudClient, Error> {
    let session = NativeCredentialStore::open()?.load()?;
    let origin = match std::env::var("REPROIT_CLOUD_ORIGIN") {
        Ok(origin) => origin,
        Err(std::env::VarError::NotPresent) => OFFICIAL_CLOUD_ORIGIN.to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => return Err(Error::schema_invalid()),
    };
    HttpCloudClient::new(&origin, session)
}

async fn initialize_command(
    current_directory: &std::path::Path,
    args: InitArgs,
) -> Result<(), Error> {
    let repository = current_git_repository(current_directory)?;
    let cloud = cloud_client()?;
    let directory = managed_service_directory(&cloud, &repository.repository_id).await?;
    let mut store = FilesystemRepository::new(repository.root.clone());
    let current = ProjectStore::read_project(&store)?;
    let service = select_initialization_service(&directory, &args, current.as_ref())?;
    let sdk = select_sdk(&args, current.as_ref())?;
    let sdk_release = released_sdk(sdk)?;
    let working_directory = repository_relative_path(&repository.root, current_directory)?;
    let service_path = select_service_path(&args, current.as_ref(), &working_directory)?;
    let run = select_startup_run(sdk, &args, current.as_ref(), &working_directory)?;
    capture_probe::verify(&repository.root, sdk, &run)?;
    let config = ProjectConfig {
        format: 1,
        organization_id: service.organization_id,
        profile: "backend".to_owned(),
        profile_format: 1,
        processing_mode: service.processing_mode,
        project_id: service.project_id,
        repository_id: repository.repository_id,
        run,
        sdk,
        service_id: service.service_id,
        service_path,
        source: ProjectSourceConfig {
            remote: repository.remote,
        },
    };
    if let Some(current) = current.as_ref()
        && current != &config
        && !args.non_interactive
    {
        render_project_diff(current, &config)?;
        if !read_confirmation()? {
            return Err(Error::new(
                ErrorCode::ConfigConflict,
                "Initialization was not confirmed.",
            ));
        }
    }
    match initialize(&mut store, &config)? {
        InitializationResult::Created | InitializationResult::Updated => {
            stdout_line(format_args!("Repro It is ready."))?;
        }
        InitializationResult::Unchanged => {
            stdout_line(format_args!("Repro It is already ready."))?;
        }
    }
    render_sdk_setup(sdk_release)?;
    Ok(())
}

async fn managed_service_directory(
    cloud: &HttpCloudClient,
    repository_id: &str,
) -> Result<InitializationDirectory, Error> {
    let mut services = Vec::new();
    let mut seen_cursors = BTreeSet::new();
    let mut catalog = cloud.list_services(repository_id).await?;
    for page_index in 0..MAX_SERVICE_CATALOG_PAGES {
        services.extend(
            catalog
                .services
                .into_iter()
                .map(|service| InitializationService {
                    organization_id: service.organization_id,
                    processing_mode: service.processing_mode,
                    project_id: service.project_id,
                    qualified_name: service.qualified_name,
                    repository_id: service.repository_id,
                    service_id: service.service_id,
                }),
        );
        let Some(cursor) = catalog.next_cursor else {
            return InitializationDirectory::new(repository_id, services);
        };
        if page_index + 1 == MAX_SERVICE_CATALOG_PAGES || !seen_cursors.insert(cursor.clone()) {
            return Err(invalid_service_catalog());
        }
        catalog = cloud
            .list_services_page(&ServiceCatalogQuery {
                cursor: Some(cursor),
                limit: Some(SERVICE_CATALOG_PAGE_SIZE),
                repository_id: repository_id.to_owned(),
            })
            .await?;
    }
    Err(invalid_service_catalog())
}

fn invalid_service_catalog() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        "The available Cloud service catalog is invalid.",
    )
}

fn select_initialization_service(
    directory: &InitializationDirectory,
    args: &InitArgs,
    current: Option<&ProjectConfig>,
) -> Result<InitializationService, Error> {
    if args.service.is_some() {
        return directory.select(args.service.as_deref());
    }
    if let Some(current) = current
        && let Some(service) = directory.services.iter().find(|service| {
            service.organization_id == current.organization_id
                && service.project_id == current.project_id
                && service.service_id == current.service_id
        })
    {
        return Ok(service.clone());
    }
    if directory.services.len() == 1 {
        return directory.select(None);
    }
    if args.non_interactive {
        return directory.select(None);
    }
    stdout_line(format_args!("Select a service:"))?;
    for service in &directory.services {
        stdout_line(format_args!("{}", service.qualified_name))?;
    }
    let selected = read_bounded_answer()?;
    directory.select(Some(&selected))
}

fn select_sdk(args: &InitArgs, current: Option<&ProjectConfig>) -> Result<BackendSdk, Error> {
    if let Some(sdk) = args.sdk {
        return Ok(sdk.into());
    }
    if let Some(current) = current {
        return Ok(current.sdk);
    }
    if args.non_interactive {
        return Err(Error::new(
            ErrorCode::ConfigConflict,
            "Non-interactive initialization requires an SDK selection.",
        ));
    }
    stdout_line(format_args!("Select an SDK:"))?;
    stdout_line(format_args!("go, nodejs, python"))?;
    match read_bounded_answer()?.as_str() {
        "dotnet" => Ok(BackendSdk::Dotnet),
        "go" => Ok(BackendSdk::Go),
        "nodejs" => Ok(BackendSdk::Nodejs),
        "python" => Ok(BackendSdk::Python),
        "rust" => Ok(BackendSdk::Rust),
        _ => Err(Error::schema_invalid()),
    }
}

fn select_run(
    args: &InitArgs,
    current: Option<&ProjectConfig>,
    working_directory: &str,
) -> Result<RunSpec, Error> {
    if let Some((program, arguments)) = args.run.split_first() {
        return Ok(RunSpec {
            arguments: arguments.to_vec(),
            program: program.clone(),
            working_directory: working_directory.to_owned(),
        });
    }
    if let Some(current) = current {
        return Ok(current.run.clone());
    }
    if args.non_interactive {
        return Err(Error::new(
            ErrorCode::ConfigConflict,
            "Non-interactive initialization requires a run program.",
        ));
    }
    stdout_line(format_args!("Run program and arguments:"))?;
    let values = split_run_answer(&read_bounded_answer()?)?;
    let (program, arguments) = values.split_first().ok_or_else(Error::schema_invalid)?;
    Ok(RunSpec {
        arguments: arguments.to_vec(),
        program: program.clone(),
        working_directory: working_directory.to_owned(),
    })
}

fn select_startup_run(
    sdk: BackendSdk,
    args: &InitArgs,
    current: Option<&ProjectConfig>,
    working_directory: &str,
) -> Result<RunSpec, Error> {
    normalize_startup_run(sdk, select_run(args, current, working_directory)?)
}

fn select_service_path(
    args: &InitArgs,
    current: Option<&ProjectConfig>,
    working_directory: &str,
) -> Result<String, Error> {
    if let Some(path) = args.service_path.as_ref() {
        return Ok(path.clone());
    }
    if let Some(current) = current {
        return Ok(current.service_path.clone());
    }
    if working_directory == "." {
        return Ok(".".to_owned());
    }
    if args.non_interactive {
        return Err(Error::new(
            ErrorCode::ConfigConflict,
            "Non-interactive initialization requires a service path from this directory.",
        ));
    }
    stdout_line(format_args!("Service path:"))?;
    read_bounded_answer()
}

fn split_run_answer(answer: &str) -> Result<Vec<String>, Error> {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut values = Vec::new();
    let mut value = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut started = false;
    for character in answer.chars() {
        if escaped {
            value.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match (quote, character) {
            (Quote::None | Quote::Double, '\\') => escaped = true,
            (Quote::None, '\'') => {
                quote = Quote::Single;
                started = true;
            }
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, '"') => {
                quote = Quote::Double;
                started = true;
            }
            (Quote::None, value_character) if value_character.is_whitespace() => {
                if started {
                    values.push(std::mem::take(&mut value));
                    started = false;
                }
            }
            (_, value_character) if !value_character.is_control() => {
                value.push(value_character);
                started = true;
            }
            _ => return Err(Error::schema_invalid()),
        }
        if value.len() > 4_096 || values.len() > 64 {
            return Err(Error::schema_invalid());
        }
    }
    if escaped || quote != Quote::None {
        return Err(Error::schema_invalid());
    }
    if started {
        values.push(value);
    }
    if values.is_empty() || values.len() > 64 {
        return Err(Error::schema_invalid());
    }
    Ok(values)
}

fn render_project_diff(current: &ProjectConfig, proposed: &ProjectConfig) -> Result<(), Error> {
    let current = toml::to_string_pretty(current).map_err(|_| evaluation_error())?;
    let proposed = toml::to_string_pretty(proposed).map_err(|_| evaluation_error())?;
    if current.len().saturating_add(proposed.len()) > 128 * 1024 {
        return Err(Error::schema_invalid());
    }
    stdout_line(format_args!("--- .reproit/project.toml"))?;
    stdout_line(format_args!("+++ .reproit/project.toml"))?;
    for line in current.lines() {
        stdout_line(format_args!("-{line}"))?;
    }
    for line in proposed.lines() {
        stdout_line(format_args!("+{line}"))?;
    }
    stdout_line(format_args!("Apply this change? [y/N]"))?;
    Ok(())
}

fn read_confirmation() -> Result<bool, Error> {
    let answer = read_bounded_answer()?;
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn read_bounded_answer() -> Result<String, Error> {
    use std::io::{BufRead as _, Read as _};

    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .take(513)
        .read_line(&mut answer)
        .map_err(|_| evaluation_error())?;
    let answer = answer.trim().to_owned();
    if answer.is_empty() || answer.len() > 512 || answer.chars().any(char::is_control) {
        return Err(Error::schema_invalid());
    }
    Ok(answer)
}

fn repository_relative_path(
    root: &std::path::Path,
    current: &std::path::Path,
) -> Result<String, Error> {
    let relative = current
        .strip_prefix(root)
        .map_err(|_| Error::schema_invalid())?;
    if relative.as_os_str().is_empty() {
        return Ok(".".to_owned());
    }
    let value = relative
        .to_str()
        .ok_or_else(Error::schema_invalid)?
        .replace('\\', "/");
    if value.starts_with('/') || value.split('/').any(|part| part.is_empty() || part == "..") {
        return Err(Error::schema_invalid());
    }
    Ok(value)
}

fn render_sdk_setup(sdk: ReleasedSdkDeclaration) -> Result<(), Error> {
    for line in sdk_setup_lines(sdk) {
        stdout_line(format_args!("{line}"))?;
    }
    Ok(())
}

fn sdk_setup_lines(sdk: ReleasedSdkDeclaration) -> [&'static str; 8] {
    [
        "Install the released SDK:",
        sdk.install_command,
        "Set REPROIT_MANAGED_PROJECT_TOKEN in your deployment secret store.",
        "Do not put the token in .reproit/project.toml.",
        "The SDK reads the token only after it captures a complete Failure.",
        "The SDK captures supported application observations automatically.",
        "Unsupported effects keep that Failure local.",
        "The SDK loads .reproit/project.toml and the current Git revision.",
    ]
}

fn evaluation_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not evaluate the command.",
    )
}

/// The echoed command name is attacker-controlled input, so it keeps only
/// printable ASCII and a bounded length before it reaches the terminal.
fn unrecognized_command_name(error: &clap::Error) -> String {
    let name = error
        .get(ContextKind::InvalidSubcommand)
        .and_then(|value| match value {
            ContextValue::String(name) => Some(name.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let sanitized: String = name
        .chars()
        .filter(|character| character.is_ascii_graphic() && *character != '\'')
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

impl From<&Command> for PublicErrorContext {
    fn from(command: &Command) -> Self {
        match command {
            Command::Check { .. } => Self::Check,
            Command::Login => Self::Login,
            Command::Init(_) => Self::Init,
            Command::Campaign(_) | Command::Keep { .. } | Command::List(_) | Command::Triage(_) => {
                Self::Cloud
            }
            Command::Debug { .. } => Self::Source,
            Command::Gate(_) | Command::Verify { .. } => Self::Release,
            Command::Mcp | Command::Remove { .. } => Self::General,
        }
    }
}

#[cfg(test)]
mod main_tests;
