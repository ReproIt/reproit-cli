use std::{collections::BTreeSet, path::Path, process::ExitCode};

mod capture_detection;
mod capture_probe;
mod go_capture_probe;
mod go_instrumentation;
mod login_command;

#[cfg(test)]
use login_command::select_login_configuration;

use clap::{Args, Parser, Subcommand, ValueEnum};
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
use reproit_cli::authored_repro::{AuthoredCheckOutcome, check_authored_repro};
use reproit_cli::cloud::HttpCloudClient;
use reproit_cli::render::{
    PublicErrorContext, parse_cli, render_error, render_observed_results, stdout_line,
};
use reproit_cli::{
    FilesystemRepository, NativeCredentialStore, current_git_repository,
    initialization::{InitializationDirectory, InitializationService},
};
use reproit_cloud_api::{Priority, ServiceCatalogQuery, Workflow};
use reproit_core::{Error, ErrorCode, identity::ReproId, model::DiscoverySource};

use capture_detection::{ReleasedSdkDeclaration, normalize_startup_run, released_sdk};

const OFFICIAL_CLOUD_ORIGIN: &str = "https://cloud.reproit.com";
const SERVICE_CATALOG_PAGE_SIZE: u8 = 50;
const MAX_SERVICE_CATALOG_PAGES: usize = 6;

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
    /// Show error codes, failed criteria, and execution problems.
    #[arg(long, global = true)]
    details: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in through your browser.
    Login,
    /// Connect this Git repository to a service and SDK.
    Init(InitArgs),
    /// List open or kept Repros.
    List(ListArgs),
    /// Change a Repro's priority, assignment, or workflow state.
    Triage(TriageArgs),
    /// Reproduce a captured failure and connect a debugger.
    Debug { repro_id: ReproId },
    /// Test one Repro or all tracked Repros against the current source.
    Check { repro_id: Option<ReproId> },
    /// Check a Repro and keep it as a regression check after it passes.
    Keep { repro_id: ReproId },
    /// Serve Repro operations to coding agents over standard input and output.
    Mcp,
    /// Remove a kept reference and retain its Cloud history.
    Remove { repro_id: ReproId },
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
    let cli = match parse_cli::<Cli>("reproit") {
        Ok(cli) => cli,
        Err(exit) => return exit,
    };
    let details = cli.details;
    let context = PublicErrorContext::from(&cli.command);
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            render_error(context, &error, details);
            ExitCode::from(error_exit_code(context, &error))
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
        Command::Check { repro_id } => check_command(&root, &agent, repro_id).await,
        Command::Keep { repro_id } => keep_command(&agent, repro_id).await,
        Command::Mcp => reproit_cli::mcp::serve(root).await,
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

async fn check_command(
    root: &Path,
    agent: &ProductionAgent,
    repro_id: Option<ReproId>,
) -> Result<(), Error> {
    if let Some(repro_id) = repro_id
        && let Some(result) = check_authored_repro(root, repro_id)?
    {
        let label = match result.outcome {
            AuthoredCheckOutcome::Pass => "PASS",
            AuthoredCheckOutcome::Regression => "REGRESSION",
            AuthoredCheckOutcome::Unknown => "UNKNOWN",
        };
        stdout_line(format_args!("{label} {repro_id}"))?;
        render_observed_results(&result.observed_results)?;
        return match result.outcome {
            AuthoredCheckOutcome::Pass => Ok(()),
            AuthoredCheckOutcome::Regression => Err(Error::new(
                ErrorCode::DifferentFailure,
                "The authored research result is outside its expected range.",
            )),
            AuthoredCheckOutcome::Unknown => Err(Error::new(
                ErrorCode::EvaluationError,
                "The authored research result is unknown.",
            )),
        };
    }
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
                    AgentCheckStatus::Unknown => "UNKNOWN",
                    AgentCheckStatus::Error => "ERROR",
                };
                stdout_line(format_args!("{label} {}", check.repro_id))
            },
        )
        .await?;
    if is_set_check {
        stdout_line(format_args!(
            "Totals: {} passed, {} regressed, {} unknown, {} errors.",
            result.pass_count, result.regression_count, result.unknown_count, result.error_count
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
    if result.unknown_count > 0 {
        return Err(Error::new(
            ErrorCode::EvaluationError,
            "At least one Repro has an unknown result.",
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
                let source = discovery_label(repro.discovery_source);
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

const fn discovery_label(source: Option<DiscoverySource>) -> &'static str {
    match source {
        Some(DiscoverySource::FuzzCampaign) => "Fuzz discovered",
        _ => "Production discovered",
    }
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

impl From<&Command> for PublicErrorContext {
    fn from(command: &Command) -> Self {
        match command {
            Command::Check { .. } => Self::Check,
            Command::Login => Self::Login,
            Command::Init(_) => Self::Init,
            Command::Keep { .. } | Command::List(_) | Command::Triage(_) => Self::Cloud,
            Command::Debug { .. } => Self::Source,
            Command::Mcp | Command::Remove { .. } => Self::General,
        }
    }
}

#[cfg(test)]
mod main_tests;
